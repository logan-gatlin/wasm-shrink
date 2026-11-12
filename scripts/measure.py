"""Measure wasm-shrink against wasm-opt over a corpus.

usage: python3 -I scripts/measure.py [--configs a,b] [--out results.json] [--profile small|fast]

corpus/manifest.json lists {id, profile, none, opt}: `none` is the worker-build
output without wasm-opt, `opt` the same build after wasm-opt (-O3 for the fast
profile, -Oz for small). Every output is validated with wasm-tools.
"""
import argparse, json, os, re, statistics as st, subprocess, tempfile
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = f"{HERE}/target/release/wasm-shrink"
SEC = re.compile(r'^\s+(custom "[^"]+"|\w+)\s+\|.*\|\s+(\d+) bytes')
# Each config is the extra command line for wasm-shrink. Single passes run without
# function reordering (the post-emit step) so each number is that pass alone.
ALONE = ["--no-reorder"]
CONFIGS = {
    "roundtrip": ["--passes", "none", *ALONE],
    "reorder": ["--passes", "none"],
    "memory-packing": ["--passes", "memory-packing", *ALONE],
    "thunks": ["--passes", "thunks", *ALONE],
    "dedupe": ["--passes", "dedupe", *ALONE],
    "merge-similar": ["--passes", "merge-similar", *ALONE],
    "inline": ["--passes", "inline", *ALONE],
    "branches": ["--passes", "branches", *ALONE],
    "fold-tails": ["--passes", "fold-tails", *ALONE],
    "addr-cse": ["--passes", "addr-cse", *ALONE],
    "redundant-sets": ["--passes", "redundant-sets", *ALONE],
    "const-hoist": ["--passes", "const-hoist", *ALONE],
    "locals": ["--passes", "stackify,coalesce-locals,stackify", *ALONE],
    "all": [],
}


def sections(path):
    out = subprocess.run(["wasm-tools", "objdump", path], capture_output=True, text=True).stdout
    d = {}
    for line in out.splitlines():
        m = SEC.match(line)
        if m:
            d[m[1]] = d.get(m[1], 0) + int(m[2])
    d["TOTAL"] = os.path.getsize(path)
    return d


def shrink(src, passes):
    with tempfile.NamedTemporaryFile(suffix=".wasm") as t:
        cmd = [BIN, src, "-o", t.name, "-q"] + passes
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            return {"error": r.stderr[-400:]}
        v = subprocess.run(["wasm-tools", "validate", "--features", "all", t.name], capture_output=True, text=True)
        if v.returncode:
            return {"error": "invalid: " + v.stderr[-400:]}
        return sections(t.name)


def pct(a, b):
    return (b / a - 1) * 100


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--configs", default=",".join(CONFIGS))
    ap.add_argument("--profile")
    ap.add_argument("--out", default=f"{HERE}/corpus/results.json")
    a = ap.parse_args()
    rows = [r for r in json.load(open(f"{HERE}/corpus/manifest.json")) if not a.profile or r["profile"] == a.profile]
    cfgs = a.configs.split(",")
    jobs = [(r, c) for r in rows for c in ["__none__", "__opt__"] + cfgs]

    def run(job):
        r, c = job
        if c == "__none__":
            return job, sections(r["none"])
        if c == "__opt__":
            return job, sections(r["opt"])
        return job, shrink(r["none"], CONFIGS[c] if c in CONFIGS else ["--passes", c, *ALONE])

    res = {}
    with ThreadPoolExecutor(16) as ex:
        for (r, c), s in ex.map(run, jobs):
            res.setdefault(f'{r["profile"]}/{r["id"]}', {})[c] = s
    json.dump(res, open(a.out, "w"), indent=0)

    nameless = lambda s: s["TOTAL"] - s.get('custom "name"', 0)
    for prof in sorted({r["profile"] for r in rows}):
        keys = [k for k in res if k.startswith(prof + "/")]
        print(f"\n== {prof} profile ({len(keys)} projects); median change vs. unoptimized input")
        print(f'{"config":16s} {"total":>8s} {"no names":>9s} {"code":>8s} {"data":>8s}  {"errors":>6s}  {"beats wasm-opt":>14s}')
        for c in ["__opt__"] + cfgs:
            ok = [k for k in keys if "error" not in res[k][c]]
            if not ok:
                print(c, "all failed:", next(iter(res[k][c]["error"] for k in keys))[:300]); continue
            med = lambda f: st.median(pct(f(res[k]["__none__"]), f(res[k][c])) for k in ok)
            beats = sum(res[k][c]["TOTAL"] <= res[k]["__opt__"]["TOTAL"] for k in ok)
            label = "wasm-opt" if c == "__opt__" else c
            print(f'{label:16s} {med(lambda s: s["TOTAL"]):8.2f} {med(nameless):9.2f} {med(lambda s: s.get("code", 0)):8.2f} '
                  f'{med(lambda s: s.get("data", 0) or 1):8.2f}  {len(keys) - len(ok):6d}  {beats:>6d}/{len(ok)}')
        errs = {k: res[k][c]["error"] for c in cfgs for k in keys if "error" in res[k][c]}
        for k, e in list(errs.items())[:5]:
            print("ERROR", k, e[:300].replace("\n", " "))


if __name__ == "__main__":
    main()
