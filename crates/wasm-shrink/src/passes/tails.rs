//! Terminating-tail folding, and removing dead code after terminators.
//!
//! `fold-tails` (the part of binaryen's `code-folding` that matters here):
//! Rust bounds checks and `unwrap`s end in panic calls such as
//!
//! ```text
//! i32.const <msg>; i32.const <len>; i32.const <loc>; call $panic; unreachable
//! ```
//!
//! and after inlining one function often holds many identical copies. Identical
//! tails that end in `unreachable` or `return`, and only use values they push
//! themselves, move to one copy at the function's outer level:
//!
//! ```text
//! block $t1            ;; one block per folded tail
//!   block $body (result …)
//!     <body, each tail replaced by `br $t1`>
//!   end
//!   return
//! end
//! <tail>
//! ```
//!
//! `dce_function`: instructions after `unreachable`, `br`, `br_table`,
//! `return`, `throw` in the same sequence never run, so they are removed.
//! `branches` runs it.

use crate::passes::branches::children;
use crate::stack::effect;
use crate::util::leb_len;
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{LocalFunction, Module};

#[derive(Default, Debug)]
pub struct Stats {
    pub tails: usize,
    pub sites: usize,
}

fn terminates(i: &Instr) -> bool {
    matches!(
        i,
        Instr::Unreachable(_)
            | Instr::Br(_)
            | Instr::BrTable(_)
            | Instr::Return(_)
            | Instr::Throw(_)
            | Instr::ThrowRef(_)
            | Instr::Rethrow(_)
            | Instr::ReturnCall(_)
            | Instr::ReturnCallIndirect(_)
            | Instr::ReturnCallRef(_)
    )
}

/// Rough encoded size of a straight-line instruction.
fn size(i: &Instr) -> usize {
    match i {
        Instr::Const(c) => match c.value {
            Value::I32(v) => 1 + leb_len(v.unsigned_abs() as u64 * 2),
            Value::I64(v) => 1 + leb_len(v.unsigned_abs() * 2),
            Value::F32(_) => 5,
            Value::F64(_) => 9,
            Value::V128(_) => 18,
        },
        Instr::Call(_) => 3,
        Instr::CallIndirect(_) => 3,
        Instr::LocalGet(_)
        | Instr::LocalSet(_)
        | Instr::LocalTee(_)
        | Instr::GlobalGet(_)
        | Instr::GlobalSet(_) => 2,
        Instr::Load(_) | Instr::Store(_) => 3,
        _ => 1,
    }
}

fn seqs(f: &LocalFunction) -> Vec<InstrSeqId> {
    let mut out = Vec::new();
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        out.push(s);
        for (i, _) in &f.block(s).instrs {
            stack.extend(children(i));
        }
    }
    out
}

/// Remove the instructions after one that never falls through, in every
/// sequence of `f`. Returns how many went.
pub(crate) fn dce_function(f: &mut LocalFunction) -> usize {
    let mut dead = 0;
    for s in seqs(f) {
        let instrs = &mut f.block_mut(s).instrs;
        if let Some(k) = instrs.iter().position(|(i, _)| terminates(i)) {
            dead += instrs.len() - k - 1;
            instrs.truncate(k + 1);
        }
    }
    dead
}

/// The start of the longest self-contained straight-line tail of `instrs`.
fn tail_start(m: &Module, f: &LocalFunction, instrs: &[(Instr, InstrLocId)]) -> Option<usize> {
    let last = &instrs.last()?.0;
    if !matches!(last, Instr::Unreachable(_) | Instr::Return(_)) {
        return None;
    }
    // Earliest start: after the last instruction that is control flow.
    let mut lo = instrs.len() - 1;
    while lo > 0 {
        let i = &instrs[lo - 1].0;
        if !children(i).is_empty()
            || terminates(i)
            || matches!(
                i,
                Instr::BrIf(_)
                    | Instr::BrOnNull(_)
                    | Instr::BrOnNonNull(_)
                    | Instr::BrOnCast(_)
                    | Instr::BrOnCastFail(_)
            )
        {
            break;
        }
        if effect(m, f, i).is_none() {
            break;
        }
        lo -= 1;
    }
    // First start from which the tail never pops a value it did not push.
    'start: for s in lo..instrs.len() {
        let mut h: isize = 0;
        for (i, _) in &instrs[s..instrs.len() - 1] {
            let (pop, push) = effect(m, f, i)?;
            h -= pop as isize;
            if h < 0 {
                continue 'start;
            }
            h += push as isize;
        }
        // `return` pops the results; `unreachable` pops nothing.
        if let Instr::Return(_) = last {
            let n = m.types.get(f.ty()).results().len() as isize;
            if h < n {
                continue 'start;
            }
        }
        return Some(s);
    }
    None
}

/// Deepest label nesting in the function.
fn max_depth(f: &LocalFunction) -> usize {
    let mut best = 0;
    let mut stack = vec![(f.entry_block(), 1usize)];
    while let Some((s, d)) = stack.pop() {
        best = best.max(d);
        for (i, _) in &f.block(s).instrs {
            for c in children(i) {
                stack.push((c, d + 1));
            }
        }
    }
    best
}

/// The tails to fold in one function: every occurrence of each, and its code.
struct Fold {
    sites: Vec<Vec<(InstrSeqId, usize)>>,
    tails: Vec<Vec<(Instr, InstrLocId)>>,
}

/// Find the tails worth folding. Reads only; [`apply`] makes the change.
fn plan(m: &Module, f: &LocalFunction) -> Option<Fold> {
    // Group tails by their exact instructions (ids are function-local or
    // module-wide, so equal text means equal code).
    // Code inside a `try`/`try_table` body may have its throws caught there;
    // moving it out would change that, so those sequences are left alone.
    let mut protected = std::collections::HashSet::new();
    for s in seqs(f) {
        for (i, _) in &f.block(s).instrs {
            let body = match i {
                Instr::TryTable(t) => Some(t.seq),
                Instr::Try(t) => Some(t.seq),
                _ => None,
            };
            if let Some(b) = body {
                let mut stack = vec![b];
                while let Some(x) = stack.pop() {
                    protected.insert(x);
                    for (j, _) in &f.block(x).instrs {
                        stack.extend(children(j));
                    }
                }
            }
        }
    }
    let mut groups: HashMap<String, Vec<(InstrSeqId, usize)>> = HashMap::new();
    let mut bytes: HashMap<String, usize> = HashMap::new();
    for s in seqs(f) {
        if protected.contains(&s) {
            continue;
        }
        let instrs = &f.block(s).instrs;
        if let Some(k) = tail_start(m, f, instrs) {
            let tail: Vec<&Instr> = instrs[k..].iter().map(|(i, _)| i).collect();
            let key = format!("{tail:?}");
            bytes
                .entry(key.clone())
                .or_insert_with(|| tail.iter().map(|i| size(i)).sum());
            groups.entry(key).or_default().push((s, k));
        }
    }
    let mut chosen: Vec<(String, Vec<(InstrSeqId, usize)>)> = groups
        .into_iter()
        .filter(|(k, v)| {
            let n = v.len() as i64;
            let b = bytes[k] as i64;
            n * b > b + 3 + n * 3 + 2
        })
        .collect();
    // Every folded tail wraps the whole body in one more block, so every branch
    // gets one level deeper; past 127 levels a branch depth takes two bytes.
    // Keep the deepest branch under that, folding the biggest savings first.
    let depth = max_depth(f);
    let room = 127usize.saturating_sub(depth + 1).min(32);
    chosen.sort_by(|a, b| {
        let sa = (a.1.len() as i64 - 1) * bytes[&a.0] as i64;
        let sb = (b.1.len() as i64 - 1) * bytes[&b.0] as i64;
        sb.cmp(&sa).then(a.0.cmp(&b.0))
    });
    chosen.truncate(room);
    if chosen.is_empty() {
        return None;
    }
    let tails = chosen
        .iter()
        .map(|(_, sites)| {
            let (s0, k0) = sites[0];
            f.block(s0).instrs[k0..].to_vec()
        })
        .collect();
    Some(Fold {
        sites: chosen.into_iter().map(|(_, v)| v).collect(),
        tails,
    })
}

fn apply(m: &mut Module, fid: walrus::FunctionId, fold: Fold, stats: &mut Stats) {
    let Fold {
        sites: chosen,
        tails,
    } = fold;
    let f = m.funcs.get(fid).kind.unwrap_local();
    let entry = f.entry_block();
    let results = m.types.get(f.ty()).results().to_vec();
    let body_ty = InstrSeqType::new(&mut m.types, &[], &results);
    let f = m.funcs.get_mut(fid).kind.unwrap_local_mut();
    // Wrap the body: entry = [block $t1 [block $t2 … [block $body <old>] return …] tail2] tail1
    let old = std::mem::take(&mut f.block_mut(entry).instrs);
    let b = f.builder_mut();
    let body = b.dangling_instr_seq(body_ty).id();
    b.instr_seq(body).instrs_mut().extend(old);
    let mut inner: Vec<(Instr, InstrLocId)> = vec![
        (Block { seq: body }.into(), InstrLocId::default()),
        (Return {}.into(), InstrLocId::default()),
    ];
    let mut labels = Vec::new();
    for tail in tails {
        let t = f
            .builder_mut()
            .dangling_instr_seq(InstrSeqType::Simple(None))
            .id();
        f.block_mut(t).instrs = inner;
        inner = vec![(Block { seq: t }.into(), InstrLocId::default())];
        inner.extend(tail);
        labels.push(t);
    }
    f.block_mut(entry).instrs = inner;
    // Replace every occurrence (later positions first within a sequence).
    let mut edits: Vec<(InstrSeqId, usize, InstrSeqId)> = Vec::new();
    for (sites, &t) in chosen.iter().zip(&labels) {
        for &(s, k) in sites {
            // Occurrences recorded in the old entry block now live in `body`.
            let s = if s == entry { body } else { s };
            edits.push((s, k, t));
        }
        stats.tails += 1;
        stats.sites += sites.len();
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.1));
    for (s, k, t) in edits {
        let instrs = &mut f.block_mut(s).instrs;
        instrs.truncate(k);
        instrs.push((Br { block: t }.into(), InstrLocId::default()));
    }
}

pub fn fold(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    // Plan in parallel; apply in function order, since applying can add a
    // block type to the module.
    for (id, plan) in crate::util::par_map(m, |_, f| plan(m, f)) {
        if let Some(fold) = plan {
            apply(m, id, fold, &mut stats);
        }
    }
    stats
}
