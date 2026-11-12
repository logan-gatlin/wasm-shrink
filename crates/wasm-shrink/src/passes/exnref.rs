//! Translate legacy exception handling (`try`, `catch`, `catch_all`,
//! `delegate`, `rethrow`) to the exnref instructions (`try_table`,
//! `throw_ref`), like binaryen's `--translate-to-exnref`. V8 rejects a module
//! that mixes the two encodings, which is what a Rust build gets when its own
//! crates use exnref and the prebuilt std uses legacy `try`.
//!
//! A `try` with handlers becomes a `try_table` whose catches branch out to one
//! block per handler; each handler follows its block:
//!
//! ```text
//! try (P → R)                block $outer (P → R)
//!   <body>                     block $c2 (P → T2)
//! catch $e                       block $c1 (P → T1)
//!   <h1>                           try_table (P → R) (catch $e $c1) (catch_all $c2)
//! catch_all                          <body>
//!   <h2>                           end
//! end                              br $outer
//!                                end
//!                                block (T1 → R) <h1> end   ;; the old handler sequence
//!                                br $outer
//!                              end
//!                              block (→ R) <h2> end
//!                            end
//! ```
//!
//! - `rethrow` needs the caught exception, so a handler that some `rethrow`
//!   targets is reached through `catch_ref`/`catch_all_ref` instead; its block
//!   also yields the `exnref`, which goes into a local, and each `rethrow` of it
//!   becomes `local.get; throw_ref`.
//! - `delegate L` rethrows the exception as if it were thrown directly inside
//!   the block labelled `L`: a `try` body's handlers catch it, a handler's own
//!   `try` does not, and the function body passes it to the caller. So the
//!   contents of `L` move into `block $done (block $d (<contents> br $done)
//!   throw_ref)`, and the delegating `try` becomes a `try_table` that sends
//!   everything to `$d` with `catch_all_ref`. Walrus gives each handler its own
//!   sequence, so a label resolves to the body or the handler it was written in.
//! - A `try` with no handlers is just a `block`.
//!
//! The body and handlers keep their sequences, so branches to them still go to
//! the same places.

use crate::passes::branches::children;
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{LocalFunction, LocalId, Module, ModuleLocals, ModuleTypes, RefType, TagId, ValType};

#[derive(Default, Debug)]
pub struct Stats {
    #[allow(dead_code)] // reported through Debug
    pub functions: usize,
    #[allow(dead_code)]
    pub tries: usize,
    #[allow(dead_code)]
    pub delegates: usize,
    #[allow(dead_code)]
    pub rethrows: usize,
}

const EXNREF: ValType = ValType::Ref(RefType::EXNREF);

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    let ids: Vec<_> = m
        .funcs
        .iter_local()
        .filter(|(_, f)| has_legacy_eh(f))
        .map(|(id, _)| id)
        .collect();
    let tags: HashMap<TagId, Vec<ValType>> = m
        .tags
        .iter()
        .map(|t| (t.id(), m.types.params(t.ty()).to_vec()))
        .collect();
    for id in ids {
        let walrus::FunctionKind::Local(f) = &mut m.funcs.get_mut(id).kind else {
            unreachable!()
        };
        translate(f, &mut m.types, &mut m.locals, &tags, &mut stats);
        stats.functions += 1;
    }
    stats
}

fn has_legacy_eh(f: &LocalFunction) -> bool {
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        for (i, _) in &f.block(s).instrs {
            if matches!(i, Instr::Try(_) | Instr::Rethrow(_)) {
                return true;
            }
            stack.extend(children(i));
        }
    }
    false
}

/// A sequence's parameter and result types.
fn signature(types: &ModuleTypes, ty: InstrSeqType) -> (Vec<ValType>, Vec<ValType>) {
    match ty {
        InstrSeqType::Simple(r) => (vec![], r.into_iter().collect()),
        InstrSeqType::MultiValue(t) => {
            let (p, r) = types.params_results(t);
            (p.to_vec(), r.to_vec())
        }
    }
}

fn translate(
    f: &mut LocalFunction,
    types: &mut ModuleTypes,
    locals: &mut ModuleLocals,
    tags: &HashMap<TagId, Vec<ValType>>,
    stats: &mut Stats,
) {
    // Resolve every depth first, while the nesting is still the original one.
    // `parent` maps each sequence to the one around it; depth 0 is the
    // sequence the instruction is in.
    let mut parent: HashMap<InstrSeqId, InstrSeqId> = HashMap::new();
    let mut tries: Vec<(InstrSeqId, usize)> = Vec::new();
    let mut rethrows: Vec<(InstrSeqId, usize, InstrSeqId)> = Vec::new();
    let mut delegate_to: HashMap<(InstrSeqId, usize), InstrSeqId> = HashMap::new();
    let mut targets: Vec<InstrSeqId> = Vec::new();
    let up = |parent: &HashMap<InstrSeqId, InstrSeqId>, mut s: InstrSeqId, depth: u32| {
        for _ in 0..depth {
            s = parent[&s];
        }
        s
    };
    let mut work = vec![f.entry_block()];
    while let Some(s) = work.pop() {
        for (k, (i, _)) in f.block(s).instrs.iter().enumerate() {
            for c in children(i) {
                parent.insert(c, s);
                work.push(c);
            }
            match i {
                Instr::Try(t) => {
                    tries.push((s, k));
                    for c in &t.catches {
                        if let LegacyCatch::Delegate { relative_depth } = c {
                            let l = up(&parent, s, *relative_depth);
                            delegate_to.insert((s, k), l);
                            if !targets.contains(&l) {
                                targets.push(l);
                            }
                        }
                    }
                }
                Instr::Rethrow(r) => rethrows.push((s, k, up(&parent, s, r.relative_depth))),
                _ => {}
            }
        }
    }

    // One `exnref` local per handler that is rethrown from.
    let mut exn: HashMap<InstrSeqId, LocalId> = HashMap::new();
    for &(_, _, h) in &rethrows {
        exn.entry(h).or_insert_with(|| locals.add(EXNREF));
    }
    // `$done` and `$d` for each sequence a `delegate` targets.
    let mut delegate_blocks: HashMap<InstrSeqId, (InstrSeqId, InstrSeqId)> = HashMap::new();
    for &l in &targets {
        let (p, r) = signature(types, f.block(l).ty);
        let done = InstrSeqType::new(types, &p, &r);
        let d = InstrSeqType::new(types, &p, &[EXNREF]);
        let b = f.builder_mut();
        let done = b.dangling_instr_seq(done).id();
        let d = b.dangling_instr_seq(d).id();
        delegate_blocks.insert(l, (done, d));
    }

    // Each `try` is replaced by exactly one instruction, so the positions
    // recorded above stay valid.
    for &(s, k) in &tries {
        let (instr, loc) = &f.block(s).instrs[k];
        let (Instr::Try(t), loc) = (instr.clone(), *loc) else {
            unreachable!()
        };
        stats.tries += 1;
        let new: Instr = if t.catches.is_empty() {
            Block { seq: t.seq }.into()
        } else if let Some(l) = delegate_to.get(&(s, k)) {
            stats.delegates += 1;
            let label = delegate_blocks[l].1;
            TryTable {
                seq: t.seq,
                catches: vec![TryTableCatch::CatchAllRef { label }],
            }
            .into()
        } else {
            handlers(f, types, tags, &t, &exn, loc)
        };
        f.block_mut(s).instrs[k].0 = new;
    }

    // `rethrow` → `local.get $exn  throw_ref`.
    let mut by_seq: HashMap<InstrSeqId, Vec<(usize, LocalId)>> = HashMap::new();
    for &(s, k, h) in &rethrows {
        by_seq.entry(s).or_default().push((k, exn[&h]));
        stats.rethrows += 1;
    }
    let mut seqs: Vec<_> = by_seq.into_iter().collect();
    seqs.sort_by_key(|(s, _)| s.index());
    for (s, mut sites) in seqs {
        sites.sort_by_key(|&(k, _)| std::cmp::Reverse(k));
        let instrs = &mut f.block_mut(s).instrs;
        for (k, local) in sites {
            let loc = instrs[k].1;
            instrs.splice(
                k..=k,
                [(LocalGet { local }.into(), loc), (ThrowRef {}.into(), loc)],
            );
        }
    }

    // Wrap each delegate target's contents so `$d` is directly inside it.
    for &l in &targets {
        let (done, d) = delegate_blocks[&l];
        let loc = InstrLocId::default();
        let mut contents = std::mem::take(&mut f.block_mut(l).instrs);
        contents.push((Br { block: done }.into(), loc));
        f.block_mut(d).instrs = contents;
        f.block_mut(done).instrs = vec![(Block { seq: d }.into(), loc), (ThrowRef {}.into(), loc)];
        f.block_mut(l).instrs = vec![(Block { seq: done }.into(), loc)];
    }
}

/// The blocks for a `try` with `catch`/`catch_all` handlers (see the module
/// docs). Returns the instruction that replaces the `try`.
fn handlers(
    f: &mut LocalFunction,
    types: &mut ModuleTypes,
    tags: &HashMap<TagId, Vec<ValType>>,
    t: &Try,
    exn: &HashMap<InstrSeqId, LocalId>,
    loc: InstrLocId,
) -> Instr {
    let body_ty = f.block(t.seq).ty;
    let (params, _) = signature(types, body_ty);
    let outer = f.builder_mut().dangling_instr_seq(body_ty).id();
    // Per handler: its sequence, the block a catch branches to, and the local
    // for its `exnref`, if it is rethrown from.
    let mut arms: Vec<(InstrSeqId, InstrSeqId, Option<LocalId>)> = Vec::new();
    let mut catches = Vec::new();
    for c in &t.catches {
        let (tag, handler) = match *c {
            LegacyCatch::Catch { tag, handler } => (Some(tag), handler),
            LegacyCatch::CatchAll { handler } => (None, handler),
            LegacyCatch::Delegate { .. } => unreachable!("a delegate has no handlers"),
        };
        let local = exn.get(&handler).copied();
        let mut results = tag.map(|tag| tags[&tag].clone()).unwrap_or_default();
        if local.is_some() {
            results.push(EXNREF);
        }
        let ty = InstrSeqType::new(types, &params, &results);
        let label = f.builder_mut().dangling_instr_seq(ty).id();
        catches.push(match (tag, local.is_some()) {
            (Some(tag), false) => TryTableCatch::Catch { tag, label },
            (Some(tag), true) => TryTableCatch::CatchRef { tag, label },
            (None, false) => TryTableCatch::CatchAll { label },
            (None, true) => TryTableCatch::CatchAllRef { label },
        });
        arms.push((handler, label, local));
    }
    // Innermost first: the `try_table`, then each handler after the block its
    // catch branches to.
    let mut inner: Vec<(Instr, InstrLocId)> = vec![
        (
            TryTable {
                seq: t.seq,
                catches,
            }
            .into(),
            loc,
        ),
        (Br { block: outer }.into(), loc),
    ];
    for (n, &(handler, label, local)) in arms.iter().enumerate() {
        f.block_mut(label).instrs = inner;
        inner = vec![(Block { seq: label }.into(), loc)];
        if let Some(local) = local {
            inner.push((LocalSet { local }.into(), loc));
        }
        inner.push((Block { seq: handler }.into(), loc));
        if n + 1 < arms.len() {
            inner.push((Br { block: outer }.into(), loc));
        }
    }
    f.block_mut(outer).instrs = inner;
    Block { seq: outer }.into()
}

#[cfg(test)]
mod tests {
    use crate::test_util::{flat, run};

    fn translate(wat: &str) -> String {
        let text = run(wat, |m| {
            super::run(m);
        });
        for legacy in ["try ", "catch ", "catch_all ", "delegate ", "rethrow "] {
            assert!(
                !format!("{text} ").contains(&format!(" {legacy}")),
                "{legacy}in {text}"
            );
        }
        text
    }

    #[test]
    fn catch_and_catch_all_branch_to_their_handlers() {
        let text = translate(
            r#"(module
              (tag $e (param i32))
              (func $f (param i32) (result i32)
                try (result i32)
                  local.get 0
                  throw $e
                catch $e
                  i32.const 1
                  i32.add
                catch_all
                  i32.const 2
                end))"#,
        );
        assert_eq!(
            text,
            flat(
                "block (result i32) block block (result i32) try_table (result i32) (catch $e 0 (;@3;)) (catch_all 1 (;@2;)) local.get 0 throw $e end br 2 end block (type 1) (param i32) (result i32) i32.const 1 i32.add end br 1 end block (result i32) i32.const 2 end end"
            )
        );
    }

    #[test]
    fn a_rethrown_handler_keeps_its_exnref() {
        let text = translate(
            r#"(module
              (tag $e)
              (func $f
                try
                  throw $e
                catch_all
                  rethrow 0
                end))"#,
        );
        assert_eq!(
            text,
            flat(
                "(local exnref) block block (result exnref) try_table (catch_all_ref 0 (;@2;)) throw $e end br 1 end local.set 0 block local.get 0 throw_ref end end"
            )
        );
    }

    #[test]
    fn delegate_rethrows_at_the_top_of_its_target() {
        let text = translate(
            r#"(module
              (tag $e)
              (func $f (result i32)
                try $outer (result i32)
                  try (result i32)
                    try (result i32)
                      throw $e
                    delegate $outer
                  catch_all
                    i32.const 1
                  end
                catch_all
                  i32.const 2
                end))"#,
        );
        assert_eq!(
            text,
            flat(
                "block (result i32) block try_table (result i32) (catch_all 0 (;@2;)) block (result i32) block (result exnref) block (result i32) block try_table (result i32) (catch_all 0 (;@7;)) try_table (result i32) (catch_all_ref 3 (;@5;)) throw $e end end br 1 end block (result i32) i32.const 1 end end br 1 end throw_ref end end br 1 end block (result i32) i32.const 2 end end"
            )
        );
    }

    #[test]
    fn delegate_to_the_function_body_reaches_the_caller() {
        let text = translate(
            r#"(module
              (tag $e)
              (func $f
                try
                  throw $e
                delegate 0))"#,
        );
        assert_eq!(
            text,
            flat(
                "block block (result exnref) try_table (catch_all_ref 0 (;@2;)) throw $e end br 1 end throw_ref end"
            )
        );
    }

    #[test]
    fn a_try_without_handlers_is_a_block() {
        let text = translate(
            r#"(module
              (func $f (result i32)
                try (result i32)
                  i32.const 1
                end))"#,
        );
        assert_eq!(text, flat("block (result i32) i32.const 1 end"));
    }

    #[test]
    fn a_try_with_parameters_passes_them_to_the_body() {
        let text = translate(
            r#"(module
              (tag $e (param i32))
              (func $f (param i32) (result i32)
                local.get 0
                try (param i32) (result i32)
                  throw $e
                catch $e
                end))"#,
        );
        assert_eq!(
            text,
            flat(
                "local.get 0 block (type 1) (param i32) (result i32) block (type 1) (param i32) (result i32) try_table (type 1) (param i32) (result i32) (catch $e 0 (;@2;)) throw $e end br 1 end block (type 1) (param i32) (result i32) end end"
            )
        );
    }
}
