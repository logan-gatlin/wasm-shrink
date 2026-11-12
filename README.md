# wasm-shrink

A small, fast replacement for `wasm-opt`'s size optimizations, built on [walrus](https://github.com/wasm-bindgen/walrus) and aimed at Rust/wasm-bindgen output (Cloudflare Workers built with `worker-build`).

Over 47 Rust Worker projects, wasm-shrink's output is smaller than wasm-opt's on every one, and wasm-shrink runs 5–7× faster:

| profile | size: wasm-opt | size: wasm-shrink | wasm-shrink ≤ wasm-opt | time: wasm-opt | time: wasm-shrink |
|---|---|---|---|---|---|
| small (opt-level `z`, vs `wasm-opt -Oz`) | −9.26% | **−11.20%** | 47/47 | 43 s | **8.3 s** (5.2× faster) |
| fast (opt-level 3, vs `wasm-opt -O3`) | −10.17% | **−12.27%** | 47/47 | 90 s | **12 s** (7.5× faster) |

Sizes are the median change in total module size against the unoptimized build. Times are for the whole corpus, one file at a time on 16 cores; per project, the median run takes 0.26× as long as wasm-opt's on small and 0.18× on fast. Names are kept, as `wasm-opt --debuginfo` keeps them. In workerd, the shrunk bundles return the same responses as the originals, and runtime and startup time are unchanged. Run `python3 -I scripts/measure.py` to reproduce the size numbers. It needs `wasm-tools` on PATH and the bundles listed in `corpus/manifest.json`.

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

Below are the passes in pipeline order. The number after each pass is how much larger the median module gets when that pass is left out of the pipeline (small / fast profile).

### `memory-packing`
Memory starts zeroed, so data segments don't need to store zero bytes. This pass trims them from the ends of each segment, and splits a segment wherever a run of zeros costs more than a new segment header. (2.09 / 1.46 percentage points)

### `thunks`
Finds wrapper functions that only forward their arguments to another function (`local.get …; call $g`) and points their callers straight at `$g`, so the wrappers and their names disappear. (0.21 / 0.01)

### `dedupe`
Merges functions whose bodies are byte-for-byte identical, repeating until no more merge. (2.85 / 1.43)

### `merge-similar`
Monomorphization leaves many copies of a function that differ only in their constants. This pass moves the shared body into one function that takes the constants as extra parameters, and each copy becomes a short call to it. (0.67 / 0.50)

### `inline`
Inlines functions that have a single caller, and small functions wherever inlining makes the module smaller. Each removed function also takes its name with it. (4.10 / 2.66)

### `addr-cse`
LLVM recomputes stack-frame addresses (`local.get $fp; i32.const 16; i32.add`) at every use. This pass computes each address once and keeps it in a local. (0.75 / 0.42)

### `fold-tails`
Shares identical tails within a function, such as repeated panic or return sequences: one copy stays and the others branch to it. (0.26 / 1.34)

### `branches`
Simplifies control flow. It turns `block … br_if … end` into `if`, and `if … br` into `if … else`. Branches that lead to another branch jump straight to its target. Unused blocks are removed, as is code that can never run. (0.94 / 2.14)

### `redundant-sets`
Removes writes of a constant to a local that already holds that constant. (0.19 / 0.28)

### `const-hoist`
A multi-byte constant used many times in a function moves into a local, which is set once at the top of the function. Each use then becomes a two-byte `local.get`. (0.56 / 0.76)

### `stackify`
Leaves a value on the stack instead of writing it to a local and reading it straight back (`local.set $x … local.get $x`). It runs before and after `coalesce-locals`. (about 0.1 / 0.1 for both runs)

### `coalesce-locals`
Lets locals whose live ranges don't overlap share one slot, and removes writes that are never read. (0.09 / 0.41)

### Function reordering
Runs on the emitted bytes. A function index costs one byte below 128 and two from 128, so the most-referenced functions get the one-byte indices. Turn it off with `--no-reorder`, for example to keep DWARF offsets valid. (0.31 / 0.25)

### `translate-to-exnref` (opt-in)
Rewrites legacy exception handling (`try`, `catch`, `delegate`, `rethrow`) as `try_table` and `throw_ref`, the same job as binaryen's `--translate-to-exnref`. This is for runtimes such as workerd, which reject modules that mix the two encodings. It is not a size pass, and the output is slightly smaller than binaryen's translation. The CLI enables it with `--translate-to-exnref` and runs it before the other passes. It changes nothing in a module without legacy instructions.
