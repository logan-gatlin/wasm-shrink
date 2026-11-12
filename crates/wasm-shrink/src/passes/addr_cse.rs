//! Address CSE: compute `$base + C` once instead of at every use.
//!
//! LLVM's wasm backend rematerializes stack-frame and field addresses
//! (`local.get $fp; i32.const 16; i32.add`) at each use, because that is free
//! on a register machine. In wasm it is 4–6 bytes each time. When `$base` is
//! never reassigned after one dominating definition, the sum is computed once
//! right after that definition and every occurrence becomes one `local.get`.
//! This is the case of binaryen's `local-cse` that matters for Rust output,
//! extended across control flow (safe because `$base` has a single definition).
//!
//! `$base` qualifies when it is a parameter that is never written, or a local
//! written exactly once, at the top level of the function body. Only
//! occurrences after that definition are rewritten.

use crate::passes::branches::children;
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{LocalFunction, LocalId, Module, ValType};

#[derive(Default, Debug)]
pub struct Stats {
    pub sums: usize,
    pub uses: usize,
}

/// Positions of one `local.get $b; i32.const C; i32.add`: (sequence, index).
type Sites = Vec<(InstrSeqId, usize)>;

fn sleb_len(v: i64) -> usize {
    let mut n = 1;
    let mut v = v;
    loop {
        let b = v & 0x7f;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            return n;
        }
        n += 1;
    }
}

/// One sum to compute once: base local, constant, where the definition goes
/// (entry-block index, before any rewriting), and every occurrence.
type Sum = (LocalId, i32, usize, Sites);

/// Find the sums worth computing once. Reads only; [`apply`] makes the change.
fn plan(m: &Module, f: &LocalFunction) -> Vec<Sum> {
    let entry = f.entry_block();
    // Writes per local, and the entry-level index of a top-level write.
    let mut writes: HashMap<LocalId, usize> = HashMap::new();
    let mut top_def: HashMap<LocalId, usize> = HashMap::new();
    // For every sequence, the entry-level index of the instruction holding it.
    let mut top_of: HashMap<InstrSeqId, usize> = HashMap::new();
    for (k, (i, _)) in f.block(entry).instrs.iter().enumerate() {
        if let Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local }) = i {
            top_def.insert(*local, k);
        }
        let mut stack = children(i);
        while let Some(s) = stack.pop() {
            top_of.insert(s, k);
            for (j, _) in &f.block(s).instrs {
                stack.extend(children(j));
            }
        }
    }
    let mut all = vec![entry];
    all.extend(top_of.keys().copied());
    for &s in &all {
        for (i, _) in &f.block(s).instrs {
            if let Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local }) = i {
                *writes.entry(*local).or_default() += 1;
            }
        }
    }
    let params: Vec<LocalId> = f.args.clone();
    // Where the sum may be computed: index in the entry block to insert before.
    let def_point = |l: LocalId| -> Option<usize> {
        match writes.get(&l).copied().unwrap_or(0) {
            0 if params.contains(&l) => Some(0),
            1 => top_def.get(&l).map(|k| k + 1),
            _ => None,
        }
    };
    // Occurrences of `local.get $b; i32.const C; i32.add`.
    let mut occ: HashMap<(LocalId, i32), Sites> = HashMap::new();
    for &s in &all {
        let instrs = &f.block(s).instrs;
        let mut k = 0;
        while k + 2 < instrs.len() {
            if let (
                Instr::LocalGet(g),
                Instr::Const(Const {
                    value: Value::I32(c),
                }),
                Instr::Binop(Binop {
                    op: BinaryOp::I32Add,
                }),
            ) = (&instrs[k].0, &instrs[k + 1].0, &instrs[k + 2].0)
                && m.locals.get(g.local).ty() == ValType::I32
                && let Some(at) = def_point(g.local)
            {
                let pos = if s == entry { k } else { top_of[&s] };
                // Must come after the definition (at index at-1).
                if pos >= at {
                    occ.entry((g.local, *c)).or_default().push((s, k));
                    k += 3;
                    continue;
                }
            }
            k += 1;
        }
    }
    // Profitability: n uses of (get + const + add) vs one computation plus n gets.
    let mut chosen: Vec<((LocalId, i32), Sites)> = occ
        .into_iter()
        .filter(|((_, c), v)| {
            let n = v.len() as i64;
            let pattern = 2 + 1 + sleb_len(*c as i64) as i64 + 1;
            let get = 2;
            n * pattern > pattern + 2 + n * get
        })
        .collect();
    chosen.sort_by_key(|((l, c), _)| (l.index(), *c));
    chosen
        .into_iter()
        .map(|((base, c), v)| (base, c, def_point(base).unwrap(), v))
        .collect()
}

fn apply(m: &mut Module, fid: walrus::FunctionId, sums: Vec<Sum>, stats: &mut Stats) {
    // New locals, then rewrite (later positions first), then insert definitions.
    let mut repl: Vec<(InstrSeqId, usize, LocalId)> = Vec::new();
    let mut defs: Vec<(usize, LocalId, LocalId, i32)> = Vec::new();
    for (base, c, at, v) in &sums {
        let t = m.locals.add(ValType::I32);
        defs.push((*at, t, *base, *c));
        for &(s, k) in v {
            repl.push((s, k, t));
        }
        stats.sums += 1;
        stats.uses += v.len();
    }
    let f = m.funcs.get_mut(fid).kind.unwrap_local_mut();
    let entry = f.entry_block();
    let params = f.args.clone();
    repl.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (s, k, t) in repl {
        f.block_mut(s).instrs.splice(
            k..k + 3,
            [(LocalGet { local: t }.into(), InstrLocId::default())],
        );
    }
    // Entry-block indices shifted by the rewrites before them; recompute the
    // definition point by finding the defining instruction again.
    defs.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, t, base, c) in defs {
        let instrs = &f.block(entry).instrs;
        let at = if params.contains(&base)
            && !instrs.iter().any(|(i, _)| {
                matches!(i, Instr::LocalSet(x) if x.local == base)
                    || matches!(i, Instr::LocalTee(x) if x.local == base)
            }) {
            0
        } else {
            instrs
                .iter()
                .position(|(i, _)| {
                    matches!(i, Instr::LocalSet(x) if x.local == base)
                        || matches!(i, Instr::LocalTee(x) if x.local == base)
                })
                .unwrap()
                + 1
        };
        let seq: [(Instr, InstrLocId); 4] = [
            (LocalGet { local: base }.into(), InstrLocId::default()),
            (
                Const {
                    value: Value::I32(c),
                }
                .into(),
                InstrLocId::default(),
            ),
            (
                Binop {
                    op: BinaryOp::I32Add,
                }
                .into(),
                InstrLocId::default(),
            ),
            (LocalSet { local: t }.into(), InstrLocId::default()),
        ];
        f.block_mut(entry).instrs.splice(at..at, seq);
    }
}

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    // Plan in parallel; apply in function order, since new locals must be
    // allocated in the same order every run.
    for (id, sums) in crate::util::par_map(m, |_, f| plan(m, f)) {
        if !sums.is_empty() {
            apply(m, id, sums, &mut stats);
        }
    }
    stats
}
