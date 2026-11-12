# wasm-shrink

The size-reducing parts of `wasm-opt -Oz`, rebuilt as [walrus](https://github.com/wasm-bindgen/walrus) passes, for Rust/wasm-bindgen output (Cloudflare Workers built with `worker-build`).

```
cargo build --release
target/release/wasm-shrink in.wasm -o out.wasm            # default pipeline
target/release/wasm-shrink in.wasm -o out.wasm --passes dedupe,inline
target/release/wasm-shrink in.wasm -o out.wasm --passes none --no-reorder   # walrus round-trip only
target/release/wasm-shrink --help                         # every option and pass
```

Names are kept, as `wasm-opt --debuginfo` (what worker-build runs) keeps them.

The workspace has two crates:

- `crates/wasm-shrink`: the library. Its only dependencies are walrus, rayon, wasmparser and wasm-encoder.
- `crates/cli` (package `wasm-shrink-cli`): the `wasm-shrink` binary, built with clap.

### As a library

```toml
[dependencies]
wasm-shrink = { path = "…/wasm-shrink/crates/wasm-shrink" }
```

```rust
// Bytes in, bytes out: the default pipeline, walrus GC, emit, then function
// reordering, which works on the emitted bytes.
let out = wasm_shrink::shrink(&wasm)?;

// On a walrus::Module you already have: the default pipeline plus GC.
// Emit it yourself, then optionally call wasm_shrink::reorder_functions on the bytes.
wasm_shrink::optimize(&mut module);

// Pass by pass, with stats. Passes also parse from their CLI names.
use wasm_shrink::Pass;
for pass in [Pass::Dedupe, "inline".parse()?, Pass::Stackify] {
    let stats = pass.run(&mut module);
    println!("{pass}: {stats}");
}
walrus::passes::gc::run(&mut module);
```

Each pass's own function and stats struct is also public, under `wasm_shrink::passes`. For example, `passes::inline::run(m, max_rounds)` takes an inlining round limit, where `Pass::Inline` uses 50.

## Results

Corpus: 47 Rust Worker projects × 2 build profiles. These are the `none` (no wasm-opt) bundles from the wasm-opt study; `corpus/manifest.json` lists them.

- **fast** profile: opt-level 3, fat LTO, 1 codegen unit; compared against `wasm-opt -O3`.
- **small** profile: opt-level `z`; compared against `wasm-opt -Oz`.

Median size change against the unoptimized input:

| | total | total excl. names | code section | data section | projects where wasm-shrink ≤ wasm-opt |
|---|---|---|---|---|---|
| small: wasm-opt -Oz | −9.26% | −7.14% | −7.59% | −4.90% | |
| small: **wasm-shrink** | **−11.20%** | **−8.52%** | **−9.11%** | −4.89% | 47/47 |
| fast: wasm-opt -O3 | −10.17% | −8.51% | −9.31% | −4.43% | |
| fast: **wasm-shrink** | **−12.27%** | **−10.86%** | **−12.04%** | −4.43% | 47/47 |

With names excluded, wasm-shrink is still the smaller of the two on 46 of 47 projects on small and on all 47 on fast.

Each pass alone (median change against the unoptimized input, small / fast). The walrus round trip on its own changes the median by −0.01% to −0.02%; walrus no longer writes empty `else` arms (wasm-bindgen/walrus#336), so passes that create `if`s, `branches` most of all, get that saving included.

| pass | total | code section | what it does |
|---|---|---|---|
| `inline` | −2.44% / −1.79% | −0.21% / +0.11% | inline single-caller functions, and tiny ones where that shrinks the module |
| `dedupe` | −2.01% / −1.82% | −1.66% / −1.93% | merge byte-identical functions |
| `memory-packing` | −1.02% / −0.72% | 0 | stop storing zero bytes of data segments |
| `addr-cse` | −0.86% / −0.55% | −1.98% / −0.91% | compute `$fp + C` once instead of at every use |
| `branches` | −0.84% / −1.40% | −1.77% / −2.35% | `block … br_if … end` → `if`; `if … br` → `if … else`; branch straight to where a branch leads; drop unused blocks and code after an instruction that never falls through |
| `merge-similar` | −0.56% / −0.77% | −1.27% / −1.24% | merge functions that differ only in constants |
| `const-hoist` | −0.46% / −1.03% | −1.02% / −1.76% | keep a multi-byte constant that a function uses many times in a local |
| `fold-tails` | −0.12% / −1.16% | −0.26% / −1.92% | share identical panic/return tails within a function |
| `stackify`, `coalesce-locals`, `stackify` | −0.26% / −0.26% | −0.52% / −0.43% | let the stack carry a value instead of a `local.set`/`local.get` pair; share locals; remove writes that are never read |
| function reordering | −0.22% / −0.39% | −0.52% / −0.69% | give the most-referenced functions the one-byte indices |
| `redundant-sets` | −0.18% / −0.26% | −0.38% / −0.40% | remove writes of a constant the local already holds |
| `thunks` | −0.16% / −0.05% | −0.08% / −0.03% | point callers of `local.get…; call $g` wrappers at `$g` |

The passes reinforce each other, so the combined result is more than the sum of the single-pass numbers. Inlining, for example, creates the duplicate tails that `fold-tails` removes, and `fold-tails` leaves the `block … end  br` shapes that `branches` then threads.

What each step is worth inside the pipeline: the default pipeline with that one step left out, as the loss in percentage points of median size (total / excl. names / code):

| step | small | fast |
|---|---|---|
| `memory-packing` | 2.09 / 2.74 / 0.00 | 1.46 / 2.38 / 0.00 |
| `thunks` | 0.21 / 0.02 / 0.07 | 0.01 / 0.00 / 0.01 |
| `dedupe` | 2.85 / 1.44 / 1.75 | 1.43 / 1.59 / 2.05 |
| `merge-similar` | 0.67 / 1.04 / 1.31 | 0.50 / 1.02 / 1.51 |
| `inline` | 4.10 / 0.56 / 0.21 | 2.66 / 0.01 / 0.05 |
| `addr-cse` | 0.75 / 1.32 / 1.49 | 0.42 / 0.64 / 0.89 |
| `fold-tails` | 0.26 / 0.22 / 0.40 | 1.34 / 2.12 / 2.53 |
| `branches` | 0.94 / 1.53 / 1.84 | 2.14 / 2.63 / 3.61 |
| `redundant-sets` | 0.19 / 0.22 / 0.34 | 0.28 / 0.24 / 0.35 |
| `const-hoist` | 0.56 / 0.95 / 0.95 | 0.76 / 1.03 / 1.33 |
| `stackify` (before `coalesce-locals`) | 0.07 / 0.09 / 0.13 | 0.06 / 0.06 / 0.13 |
| `coalesce-locals` | 0.09 / 0.15 / 0.35 | 0.41 / 0.54 / 0.79 |
| `stackify` (after) | 0.03 / 0.03 / 0.06 | 0.02 / 0.00 / 0.06 |
| function reordering | 0.31 / 0.49 / 0.56 | 0.25 / 0.31 / 0.51 |

Every step pays for itself on at least one profile. `thunks` matters on small, where it removes whole functions and their names. The pipeline used to have 17 steps. Four were dropped because leaving them out cost nothing: a separate `dce` step (`branches` removes the same dead code), a `branches` run before `addr-cse` (leaving it out made the output smaller, by 21 KB over the fast corpus, as the run after `fold-tails` does its work), and a final `dedupe` and `thunks` (no change, or a few bytes). That made wasm-shrink about 13% faster.

Run `python3 -I scripts/measure.py` to reproduce. It needs `wasm-tools` on PATH and the bundles listed in the manifest.

### Correctness

Every output passes `wasm-tools validate`. All 42 projects that have a runtime workload were also run in workerd with the original and the shrunk bundle, on both profiles. Every deterministic response (585 of 646 requests on each profile) was byte-identical, and every status code matched. Two `rs-benchmark` responses include a measured `duration_ms`, so they vary even between runs of the original bundle. When two of the original's runs happen to agree, the checker treats that response as deterministic and can flag it as a mismatch. Every other field matched. The checker lives with the study harness, not in this repo.

Output is deterministic: two runs give the same bytes, and so does a single-threaded run (`RAYON_NUM_THREADS=1`), on all 94 corpus modules.

### Runtime and startup

Measured with the study's harness: runtime is the steady-state CPU time of each CPU-bound project's workload in workerd (14 projects × 2 profiles, 8 fresh-process runs per variant, interleaved, pinned to four performance cores), and startup is the CPU time to compile and instantiate each module in V8 (47 × 2 projects, 50 repetitions). Changes are the mean of per-project paired differences, ± standard error.

| comparison | runtime | startup total | compile |
|---|---|---|---|
| this pipeline vs the first-round pipeline | −0.05% ± 0.38 | −0.13% ± 0.05 | −0.31% ± 0.04 |
| function reordering on vs off | | +0.06% ± 0.02 | +0.11% ± 0.02 |
| trimmed 13-step vs 17-step pipeline | +0.10% ± 0.41 | −0.07% | −0.08% |

- **Runtime is unchanged.** No project moved by more than its noise in a way that repeated across runs, with one exception still being watched: `const-hoist` measured +1.5% to +2.9% on `redirectionio` (fast profile) in three runs, at about 2σ each. Across all projects `const-hoist` measures +0.29% ± 0.35.
- **Startup is slightly faster** than with the first-round pipeline, because there is less code to compile (fast −0.39% ± 0.06; small +0.14% ± 0.05). Function reordering costs about 0.1% of compile time: walrus emits the largest functions first so that a parallel compile starts on them early, and reordering gives that up. The worst single project is +1.3% total startup.
- `perf` is not available on the benchmark machine, so these are wall-clock CPU times, not instruction counts.

## Where the passes come from

The passes are chosen from a skip-one sweep over `wasm-opt -Oz`: the full pipeline with one pass skipped at a time (`--skip-pass`), on the small profile. The marginal loss per binaryen pass, in percentage points of median size:

| binaryen pass | total | excl. names | code |
|---|---|---|---|
| inlining-optimizing | 4.06 | 0.66 | 0.63 |
| duplicate-function-elimination | 1.98 | 0.67 | 1.06 |
| memory-packing | 1.28 | 1.95 | 0.00 |
| remove-unused-brs | 0.87 | 1.22 | 1.54 |
| merge-similar-functions | 0.58 | 0.65 | 1.08 |
| optimize-instructions | 0.54 | 0.60 | 1.05 |
| coalesce-locals | 0.52 | 0.54 | 0.82 |
| vacuum / simplify-locals / local-cse / code-folding | 0.1–0.4 each | | |
| every other pass | < 0.1 | | |

Two things stand out:

- **Most of binaryen's inlining gain is in the name section.** Every removed function also drops its mangled name, which averages about 120 bytes. wasm-shrink counts those name bytes only when inlining a function that has a single call site. If the inliner also counts them for functions with many call sites (`WS_INLINE_NAMES=2`), the small profile drops to −11.3% total but the code section grows 3.3%. That trade stops paying off as soon as names are stripped.
- **LLVM's wasm output re-computes stack-frame addresses at every use** (`local.get $fp; i32.const 16; i32.add`). `addr-cse` is the largest single code-section win (−1.9% code on the small profile).

### The second round

The first round left wasm-shrink behind wasm-opt on code size. The second round's passes came from two places:

- **What binaryen still removed from wasm-shrink's output**, one binaryen pass at a time (the list under [Not done](#not-done-yet) is what remains of it). This gave `redundant-sets` (binaryen's `rse`), the `branches` additions (`remove-unused-brs`), dead-write removal and stack carrying (`simplify-locals`, and the `local2stack` step of binaryen's binary writer).
- **Encoding costs that `-Oz` leaves alone.** A function index or a constant costs as many bytes as its LEB128 needs, and neither LLVM nor `-Oz` picks them with that in mind. This gave `const-hoist` and function reordering. Binaryen has both (`const-hoisting`, `reorder-functions`) but runs neither at `-Oz`.

What each piece is worth inside the pipeline: the full pipeline with that one piece left out, as the loss in percentage points of median size (total / excl. names / code). These were measured on an intermediate build that still had the 17-step pipeline; parts of a pass, such as jump threading, can't be left out from the command line.

| piece | small | fast |
|---|---|---|
| `const-hoist` | 0.56 / 0.96 / 0.95 | 0.76 / 1.00 / 1.31 |
| jump threading in `branches` | 0.06 / 0.08 / 0.16 | 0.62 / 0.81 / 1.10 |
| function reordering | 0.29 / 0.48 / 0.55 | 0.26 / 0.29 / 0.50 |
| carrying values on the stack in `stackify` | 0.22 / 0.45 / 0.52 | 0.24 / 0.28 / 0.56 |
| `if … br` → `if … else` in `branches` | 0.19 / 0.33 / 0.46 | 0.28 / 0.27 / 0.36 |
| `redundant-sets` | 0.19 / 0.23 / 0.34 | 0.27 / 0.25 / 0.34 |
| dead-write removal in `coalesce-locals` | 0.06 / 0.08 / 0.12 | 0.08 / 0.06 / 0.14 |
| `stackify` also before `coalesce-locals` | 0.07 / 0.08 / 0.12 | 0.05 / 0.06 / 0.12 |
| **the whole second round** | 1.58 / 2.25 / 2.76 | 2.58 / 3.03 / 4.10 |

Function reordering changes nothing in the projects that import 128 or more functions (11 on fast, 13 on small): there every defined function needs a two-byte index wherever it goes.

## Design notes

- **Comparing function bodies.** `dedupe` and `merge-similar` compare functions in walrus's own encoding: `util::encode_functions` emits the module once and reads every body back. No structural hash has to be kept in sync with the IR.
  `dedupe` encodes only once. It cuts each body's call targets out of the bytes and resolves them through the merges so far, so later rounds don't re-emit the module.
- **Inliner sizes and order.** The inliner estimates sizes from walrus's IR (`size.rs`) instead of emitting the module. It visits candidates callee-first, so a chain of single-caller functions collapses in one walk. Each function's gain is re-checked from its current body just before it is inlined.
- **Inliner locals.** The inlined body gets fresh locals. It zeroes them only when the call site is inside a loop and the callee might read them before writing them. Arguments that are already a plain `local.get` of a parameter the callee never writes are passed through without a copy.
- **Coalescing.** `coalesce-locals` is liveness-based colouring over the structured control-flow graph. Calls and throws inside a `try_table` get edges to the catch targets. Functions with legacy `try` are skipped.
- **Pipeline order.** `branches` runs once, after `fold-tails`, which leaves `block … end  br $tail` behind for it to thread; it also removes the dead code `inline` leaves. `redundant-sets` runs before `const-hoist`, while the constants it looks for are still constants. `stackify` runs on both sides of `coalesce-locals`: before, it turns a `local.set` into a `local.tee` wherever the stack can carry the value, and `coalesce-locals` then removes the tees whose local is never read again.
- **Carrying values on the stack.** `stackify` turns `local.set $x  <A>  local.get $x` into `local.tee $x  <A>` (or just `<A>`) when `<A>` is balanced and never pops below where it started. The value waits on the stack under `<A>`. No instruction moves, so there is no evaluation order to reason about and no effect analysis; `<A>` only must not write `$x`.
- **Constant hoisting.** A constant moves to a local when that saves bytes after paying for the `local.set`, the `local.get`s and, if needed, the declaration: for a 5-byte address, from the third use, or the fourth if the function has no local of that type yet. The local is written once at the top of the function, so every `local.get` yields the constant. There is no limit on locals per function. Capping it at four hoisted constants per function would keep about three quarters of the gain (it would lose 0.19 / 0.26 pp of code, small / fast); that is the fallback if the `redirectionio` slowdown under [Runtime and startup](#runtime-and-startup) turns out to be real.
- **Dataflow.** `coalesce-locals` (liveness, backwards) and `redundant-sets` (which constant a local holds, forwards) share one control-flow graph builder and both use bit sets, so neither needs more than a worklist loop.
- **Function reordering works on the emitted bytes.** Walrus chooses its own function order (largest first) and offers no control over it. `reorder_functions` re-encodes the module with `wasm-encoder`, sending every function index through one permutation: calls, `ref.func`, element segments, exports, the start function and the name section. A module that would not get smaller is returned untouched. It moves code offsets, so it should not run on a module whose DWARF you want to keep (`--no-reorder`).
- **Parallelism.** Per-function work runs on all cores (rayon, plus walrus's `parallel` feature for parsing and emitting). Passes that add locals or types plan in parallel and apply in function order. Walrus emits locals in `LocalId` order, so allocating them in the same order every run keeps the output deterministic, and the same as a sequential run.
- **Walrus version.** wasm-shrink pins walrus to a commit on main after 0.28.0. It needs wasm-bindgen/walrus#336, which stops the emitter writing empty `else` arms: wasm-shrink used to remove them by rewriting the code section after emit (42,919 of them in one module). #337 makes `VisitorMut` visit each field once; the two visitors here give the same result either way. Switch back to a crates.io version once one includes both.

## Not done (yet)

- **Converting legacy exception handling to exnref, which still needs wasm-opt.**
  - `worker-build --emscripten` compiles user crates with `-wasm-use-legacy-eh=false`, but the prebuilt Rust std uses legacy `try`/`catch`.
  - V8 rejects a module that mixes the two encodings, so emcc runs binaryen's `--translate-to-exnref` at link (`-sBINARYEN_EXTRA_PASSES`).
  - In debuginfo builds, binaryen must also rewrite the DWARF to follow the changed code. That step is slow, and before wasm-bindgen#5328 it failed outright.
  - wasm-shrink doesn't convert: it leaves legacy `try` alone, and `coalesce-locals` skips those functions. The corpus never needs the conversion, because the study left out `--emscripten` builds. No input contains a legacy `try`, and the 52 that use exceptions already use `try_table`.
  - This conversion may be better done in walrus, keeping debug info as source maps rather than rewriting DWARF. Walrus can't do either today. It has no translation pass, and its debug-info support is DWARF rewriting through gimli (`ModuleConfig::generate_dwarf`).
- What binaryen could still remove from wasm-shrink's output, one pass at a time (median code-section change, small / fast):
  - `code-folding` −0.14% / −0.20%. Jump threading took most of what a simple version would find. The rest is identical code before `br`s to the same block and before `unreachable`s in different blocks, which needs binaryen's search for the best set of tails to merge.
  - `simplify-locals-nostructure` −0.15% / −0.12%: moving a `local.set`'s value down to its only `local.get`. Unlike stack carrying, this reorders evaluation, so it needs an effect analysis (what each instruction reads, writes and may trap on).
  - `dae-optimizing` (dead and constant arguments) −0.05% / −0.14%. It rewrites signatures and every call site, and has to prove a function is never called indirectly.
  - `optimize-instructions` −0.08% / +0.03%, `local-cse` −0.05% / −0.03%, and under 0.03% each for `rse`, `remove-unused-brs`, `merge-blocks`, `vacuum` and `precompute`.
  - Binaryen's `optimize-added-constants` (fold `x + C` into load/store offsets) would save about 0.3% more, but it is only valid under its `--low-memory-unused` assumption, which worker-build does not make, so it is left out.
- Tried in the second round and dropped:
  - Removing a `br` wherever control would reach its target anyway (through the ends of several enclosing blocks, and a final `return`). Once `if … br` → `if … else` exists it finds 800 bytes in the whole corpus (0.003%), so only the original rule (a `br` to the end of its own block) is kept.
  - Preferring to give a copy's source and destination the same slot in `coalesce-locals`: −0.02%.
  - A second `merge-similar` at the end of the pipeline: −0.02% to −0.04%.
  - `if` → `select`: about −0.2% on the fast profile, but it makes both arms run unconditionally, so it would need the effect analysis above and a runtime benchmark per shape.
  - Giving `if` and `block` a result instead of writing a local in each arm (the "structure" half of `simplify-locals`): about −0.25%, but it too needs the effect analysis.
  - Reordering types: a module has at most 139 types, and only 5 of the 94 have any type index that needs two bytes.
- `merge-similar` merges only constant differences. Binaryen can also merge functions that call different targets, using `call_ref`.
- Speed: over the whole corpus, run one file at a time on 16 cores, wasm-shrink takes 12 s against 90 s for `wasm-opt -O3` (fast profile), and 8.3 s against 43 s for `wasm-opt -Oz` (small). The slowest file takes 1.1 s. Per project, the median run takes about 0.18× as long as wasm-opt's on fast and 0.26× on small. The second round made wasm-shrink about 1.2× slower than it was: `stackify` and `branches` each walk a function several times until nothing changes, and function reordering parses and re-encodes the module once more. `dedupe` and `merge-similar` still emit the module to compare function bodies exactly.
