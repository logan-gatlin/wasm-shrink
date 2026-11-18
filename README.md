# wasm-shrink

A small, fast replacement for `wasm-opt`'s size optimizations, built on [walrus](https://github.com/wasm-bindgen/walrus) and aimed at Rust/wasm-bindgen output (Cloudflare Workers built with `worker-build`).

Over 47 Rust Worker projects, wasm-shrink's output is smaller than wasm-opt's on all but one, and wasm-shrink runs 5–7× faster:

| profile | size: wasm-opt | size: wasm-shrink | wasm-shrink ≤ wasm-opt | time: wasm-opt | time: wasm-shrink |
|---|---|---|---|---|---|
| small (opt-level `z`, vs `wasm-opt -Oz`) | −7.14% | **−8.69%** | 46/47 | 43 s | **8.3 s** (5.2× faster) |
| fast (opt-level 3, vs `wasm-opt -O3`) | −8.51% | **−10.97%** | 47/47 | 90 s | **12 s** (7.5× faster) |

Sizes are the median change in module size against the unoptimized build, not counting the name section. The name section and debug info ship separately from the module, so no pass is tuned for them. Names are still kept and stay correct for the output, as `wasm-opt --debuginfo` keeps them, and the same code comes out whether or not the input has names. On the one project where wasm-opt wins (axum-adapter, small), its output is 4 bytes smaller. Times are for the whole corpus, one file at a time on 16 cores; per project, the median run takes 0.26× as long as wasm-opt's on small and 0.18× on fast. In workerd, the shrunk bundles return the same responses as the originals, and runtime and startup time are unchanged. Run `python3 -I scripts/measure.py` to reproduce the size numbers. It needs `wasm-tools` on PATH and the bundles listed in `corpus/manifest.json`.

Gzip is a different story. Without names, wasm-shrink's output compresses 1.05% (small) and 0.86% (fast) worse than its input, at the median, and worse than wasm-opt's output on most projects. Function reordering, `const-hoist`, `memory-packing` and `inline` each make the module smaller raw but larger after gzip; see their sections.

## Usage

```
cargo build --release
target/release/wasm-shrink in.wasm -o out.wasm                       # default pipeline
target/release/wasm-shrink in.wasm -o out.wasm --passes dedupe,inline
target/release/wasm-shrink --help                                    # every option and pass
```

As a library:

```rust
// Bytes in, bytes out.
let out = wasm_shrink::shrink(&wasm)?;

// Or one pass at a time on a walrus::Module, with stats.
use wasm_shrink::Pass;
for pass in [Pass::Dedupe, Pass::Inline, Pass::Stackify] {
    let stats = pass.run(&mut module);
    println!("{pass}: {stats}");
}
```

## Passes

Below are the passes in pipeline order. The number after each pass is how much larger the median module, without its name section, gets when that pass is left out of the pipeline (small / fast profile). Leaving out any one pass makes no project smaller, except `inline` (see below).

### `memory-packing`
Memory starts zeroed, so data segments don't need to store zero bytes. This pass trims them from the ends of each segment, and splits a segment wherever a run of zeros costs more than a new segment header. (1.55 / 1.06 percentage points; after gzip it costs 0.36 / 0.27, since runs of zeros compress to almost nothing)

### `thunks`
Finds wrapper functions that only forward their arguments to another function (`local.get …; call $g`) and points their callers straight at `$g`, so the wrappers disappear. Unlike `inline`, it also redirects table slots and exports. (0.03 / 0.003)

### `dedupe`
Merges functions whose bodies are byte-for-byte identical, repeating until no more merge. (1.08 / 1.26)

### `merge-similar`
Monomorphization leaves many copies of a function that differ only in their constants. This pass moves the shared body into one function that takes the constants as extra parameters, and each copy becomes a short call to it. (0.70 / 0.87)

### `addr-cse`
LLVM recomputes stack-frame addresses (`local.get $fp; i32.const 16; i32.add`) at every use. This pass computes each address once and keeps it in a local. It runs before `inline` and `fold-tails`, because it only sees a base defined at the top level of a function, and both of those passes nest code. (1.09 / 0.63)

### `inline`
Inlines functions that have a single caller, and small functions wherever inlining makes the module smaller. Its estimate counts only bytes that ship, so a removed function's name is not counted as a saving. (0.27 / 0.14; leaving it out makes 1 small and 7 fast projects smaller, and after gzip it costs 0.21 / 0.53)

### `fold-tails`
Shares identical tails within a function, such as repeated panic or return sequences: one copy stays and the others branch to it. (0.17 / 1.56)

### `branches`
Simplifies control flow. It turns `block … br_if … end` into `if`, and `if … br` into `if … else`. Branches that lead to another branch jump straight to its target. Unused blocks are removed, as is code that can never run. (1.27 / 2.71)

### `redundant-sets`
Removes writes of a constant to a local that already holds that constant. (0.21 / 0.29)

### `const-hoist`
A multi-byte constant used many times in a function moves into a local, which is set once at the top of the function. Each use then becomes a two-byte `local.get`. (0.50 / 0.87; after gzip it costs 0.42 / 0.74)

### `stackify`
Leaves a value on the stack instead of writing it to a local and reading it straight back (`local.set $x … local.get $x`). It runs before and after `coalesce-locals`. (0.08 / 0.08 for the first run, 0.02 / 0.01 for the second)

### `coalesce-locals`
Lets locals whose live ranges don't overlap share one slot, and removes writes that are never read. (0.12 / 0.21)

### Function reordering
Decides the order walrus emits functions in, through its `function_order` hook, so DWARF stays valid. A function index costs one byte below 128 and two from 128, so the most-referenced functions get the one-byte indices. Turn it off with `--no-reorder` to keep walrus's order (largest first). (0.26 / 0.37; after gzip it costs 0.93 / 0.99)

### `translate-to-exnref` (opt-in)
Rewrites legacy exception handling (`try`, `catch`, `delegate`, `rethrow`) as `try_table` and `throw_ref`, the same job as binaryen's `--translate-to-exnref`. This is for runtimes such as workerd, which reject modules that mix the two encodings. It is not a size pass, and the output is slightly smaller than binaryen's translation. The CLI enables it with `--translate-to-exnref` and runs it before the other passes. It changes nothing in a module without legacy instructions.
