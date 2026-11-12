//! The size-reducing parts of `wasm-opt -Oz`, as passes over a [`walrus::Module`].
//!
//! The quickest way in is [`shrink`], which takes a module's bytes and returns
//! the optimized bytes:
//!
//! ```no_run
//! let wasm = std::fs::read("in.wasm").unwrap();
//! let out = wasm_shrink::shrink(&wasm).unwrap();
//! ```
//!
//! To work on a module you already hold, call [`optimize`] and emit the module
//! yourself; [`reorder_functions`] then works on the emitted bytes. To run a subset of passes or report what each one did, run
//! [`Pass`]es one at a time:
//!
//! ```no_run
//! # let mut module = walrus::Module::default();
//! use wasm_shrink::Pass;
//! for pass in Pass::DEFAULT {
//!     let stats = pass.run(&mut module);
//!     eprintln!("{pass}: {stats}");
//! }
//! walrus::passes::gc::run(&mut module);
//! ```
//!
//! Output is deterministic: the same input always produces the same bytes,
//! even though the passes run in parallel.

pub mod passes;
mod reorder;
mod size;
mod stack;
#[cfg(test)]
mod test_util;
mod util;

pub use reorder::reorder_functions;

use passes::*;
use std::fmt;
use std::str::FromStr;
use walrus::Module;

/// One optimization pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Pass {
    /// Drop the zero bytes of active data segments ([`memory_packing`]).
    MemoryPacking,
    /// Replace calls to forwarding thunks with calls to their target ([`thunks`]).
    Thunks,
    /// Merge functions with identical bodies ([`dedupe`]).
    Dedupe,
    /// Merge functions that differ only in constants ([`merge_similar`]).
    MergeSimilar,
    /// Inline functions where that makes the module smaller ([`inline`]).
    Inline,
    /// Simplify branches, remove the ones that go where control already
    /// does, and remove code after instructions that never fall through
    /// ([`branches`]).
    Branches,
    /// Compute repeated `$base + C` addresses once ([`addr_cse`]).
    AddrCse,
    /// Share identical code that ends in a terminator ([`tails::fold`]).
    FoldTails,
    /// Remove writes of a constant a local already holds ([`locals::redundant`]).
    RedundantSets,
    /// Keep a constant used many times in a local ([`const_hoist`]).
    ConstHoist,
    /// Reuse locals whose lifetimes don't overlap, and remove writes that
    /// are never read ([`locals::coalesce`]).
    CoalesceLocals,
    /// Remove `local.set`/`local.get` pairs the stack can carry ([`locals::stackify`]).
    Stackify,
}

impl Pass {
    /// Every pass, once each.
    pub const ALL: &[Pass] = &[
        Pass::MemoryPacking,
        Pass::Thunks,
        Pass::Dedupe,
        Pass::MergeSimilar,
        Pass::Inline,
        Pass::Branches,
        Pass::AddrCse,
        Pass::FoldTails,
        Pass::RedundantSets,
        Pass::ConstHoist,
        Pass::CoalesceLocals,
        Pass::Stackify,
    ];

    /// The default pipeline, in order. Some passes appear twice.
    pub const DEFAULT: &[Pass] = &[
        Pass::MemoryPacking,
        Pass::Thunks,
        Pass::Dedupe,
        Pass::MergeSimilar,
        Pass::Inline,
        Pass::AddrCse,
        // After addr-cse: folding nests the body, which hides top-level definitions.
        Pass::FoldTails,
        // After fold-tails: a folded tail leaves `block … end  br $tail`
        // behind, which branches threads. Running branches before addr-cse as
        // well made the output larger. Branches also removes the dead code
        // inlining leaves.
        Pass::Branches,
        // Before const-hoist, which turns the constants into `local.get`s.
        Pass::RedundantSets,
        Pass::ConstHoist,
        // Before coalesce-locals: stackify turns `local.set` into `local.tee`
        // where the stack can carry the value, and coalesce-locals then
        // removes the tees whose local is not read again.
        Pass::Stackify,
        Pass::CoalesceLocals,
        Pass::Stackify,
        // A second coalesce-locals after stackify changed no output in the
        // corpus, and neither did running dedupe and thunks again at the end.
    ];

    /// The pass's command-line name, such as `coalesce-locals`.
    pub fn name(self) -> &'static str {
        match self {
            Pass::MemoryPacking => "memory-packing",
            Pass::Thunks => "thunks",
            Pass::Dedupe => "dedupe",
            Pass::MergeSimilar => "merge-similar",
            Pass::Inline => "inline",
            Pass::Branches => "branches",
            Pass::AddrCse => "addr-cse",
            Pass::FoldTails => "fold-tails",
            Pass::RedundantSets => "redundant-sets",
            Pass::ConstHoist => "const-hoist",
            Pass::CoalesceLocals => "coalesce-locals",
            Pass::Stackify => "stackify",
        }
    }

    /// Run the pass on `m`.
    ///
    /// Passes can leave unused items behind; run [`walrus::passes::gc`]
    /// after the last one.
    pub fn run(self, m: &mut Module) -> Stats {
        match self {
            Pass::MemoryPacking => Stats::MemoryPacking(memory_packing::run(m)),
            Pass::Thunks => Stats::Thunks(thunks::run(m)),
            Pass::Dedupe => Stats::Dedupe(dedupe::run(m)),
            Pass::MergeSimilar => Stats::MergeSimilar(merge_similar::run(m)),
            Pass::Inline => Stats::Inline(inline::run(m, 50)),
            Pass::Branches => Stats::Branches(branches::run(m)),
            Pass::AddrCse => Stats::AddrCse(addr_cse::run(m)),
            Pass::FoldTails => Stats::FoldTails(tails::fold(m)),
            Pass::RedundantSets => Stats::RedundantSets(locals::redundant(m)),
            Pass::ConstHoist => Stats::ConstHoist(const_hoist::run(m)),
            Pass::CoalesceLocals => Stats::CoalesceLocals(locals::coalesce(m)),
            Pass::Stackify => Stats::Stackify(locals::stackify(m)),
        }
    }
}

impl fmt::Display for Pass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Pass {
    type Err = UnknownPass;

    fn from_str(s: &str) -> Result<Pass, UnknownPass> {
        Pass::ALL
            .iter()
            .copied()
            .find(|p| p.name() == s)
            .ok_or_else(|| UnknownPass(s.to_string()))
    }
}

/// A pass name that [`Pass::from_str`] doesn't know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownPass(pub String);

impl fmt::Display for UnknownPass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown pass `{}` (known:", self.0)?;
        for p in Pass::ALL {
            write!(f, " {p}")?;
        }
        f.write_str(")")
    }
}

impl std::error::Error for UnknownPass {}

/// What one [`Pass::run`] did. `Display` prints the counts.
#[derive(Debug)]
#[non_exhaustive]
pub enum Stats {
    MemoryPacking(memory_packing::Stats),
    Thunks(thunks::Stats),
    Dedupe(dedupe::Stats),
    MergeSimilar(merge_similar::Stats),
    Inline(inline::Stats),
    Branches(branches::Stats),
    AddrCse(addr_cse::Stats),
    FoldTails(tails::Stats),
    RedundantSets(locals::RedundantStats),
    ConstHoist(const_hoist::Stats),
    CoalesceLocals(locals::CoalesceStats),
    Stackify(locals::StackifyStats),
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stats::MemoryPacking(s) => write!(f, "{s:?}"),
            Stats::Thunks(s) => write!(f, "{s:?}"),
            Stats::Dedupe(s) => write!(f, "{s:?}"),
            Stats::MergeSimilar(s) => write!(f, "{s:?}"),
            Stats::Inline(s) => write!(f, "{s:?}"),
            Stats::FoldTails(s) => write!(f, "{s:?}"),
            Stats::Branches(s) => write!(f, "{s:?}"),
            Stats::AddrCse(s) => write!(f, "{s:?}"),
            Stats::RedundantSets(s) => write!(f, "{s:?}"),
            Stats::ConstHoist(s) => write!(f, "{s:?}"),
            Stats::CoalesceLocals(s) => write!(f, "{s:?}"),
            Stats::Stackify(s) => write!(f, "{s:?}"),
        }
    }
}

/// Run the [default pipeline](Pass::DEFAULT), then walrus's GC.
pub fn optimize(m: &mut Module) {
    for pass in Pass::DEFAULT {
        pass.run(m);
    }
    walrus::passes::gc::run(m);
}

/// Parse `wasm`, [`optimize`] it, emit it and [put the most-used functions
/// first](reorder_functions).
pub fn shrink(wasm: &[u8]) -> walrus::Result<Vec<u8>> {
    let mut m = walrus::ModuleConfig::new().parse(wasm)?;
    optimize(&mut m);
    Ok(reorder_functions(&m.emit_wasm())?.0)
}

#[cfg(test)]
mod tests {
    use super::Pass;

    #[test]
    fn pass_names_round_trip() {
        for &p in Pass::ALL {
            assert_eq!(p.name().parse::<Pass>(), Ok(p));
        }
        assert!("no-such-pass".parse::<Pass>().is_err());
    }

    #[test]
    fn default_pipeline_uses_known_passes() {
        for p in Pass::DEFAULT {
            assert!(Pass::ALL.contains(p), "{p}");
        }
        for p in [Pass::RedundantSets, Pass::ConstHoist] {
            assert!(Pass::DEFAULT.contains(&p), "{p}");
        }
    }

    /// The whole pipeline on a function with something for most passes: the
    /// result validates and its code is smaller than a walrus round trip's.
    #[test]
    fn shrink_validates() {
        let wasm = wat::parse_str(
            "(module (memory 1)
               (func $g (param i32) (result i32) local.get 0)
               (func $f (export \"f\") (param i32) (result i32) (local i32 i32)
                 i32.const 0  local.set 1
                 block
                   local.get 0  if
                     i32.const 1048576  call $g  local.set 2
                     local.get 0  i32.const 1048576  i32.store
                     local.get 2  local.set 1
                     br 1
                   end
                   i32.const 1048576  call $g  local.set 1
                   i32.const 1048576  call $g  drop
                 end
                 local.get 1))",
        )
        .unwrap();
        let out = super::shrink(&wasm).unwrap();
        crate::test_util::validate(&out);
        let round_trip = walrus::Module::from_buffer(&wasm).unwrap().emit_wasm();
        assert!(
            out.len() < round_trip.len(),
            "{} -> {}",
            round_trip.len(),
            out.len()
        );
        // The four uses of the address share one constant, and the branch
        // out of the `if` became an `else`.
        let text = crate::test_util::function_text(&out, "f");
        assert_eq!(text.matches("i32.const 1048576").count(), 1, "{text}");
        assert!(text.contains(" else ") && !text.contains("br "), "{text}");
    }
}
