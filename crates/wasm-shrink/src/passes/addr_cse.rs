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
use crate::size::sleb_len;
use crate::util::{MAX_LOCALS, locals_that_fit};
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
    // Every local is a parameter or mentioned by an instruction, so there is
    // nearly always room without counting them.
    let instrs: usize = all.iter().map(|s| f.block(*s).instrs.len()).sum();
    if f.args.len() + instrs + chosen.len() > MAX_LOCALS {
        chosen.truncate(locals_that_fit(f, chosen.len()));
    }
    chosen
        .into_iter()
        .map(|((base, c), v)| (base, c, def_point(base).unwrap(), v))
        .collect()
}

fn apply(m: &mut Module, fid: walrus::FunctionId, sums: Vec<Sum>, stats: &mut Stats) {
    // New locals, then rewrite each sequence once, then insert the definitions.
    let mut repl: HashMap<InstrSeqId, Vec<(usize, LocalId)>> = HashMap::new();
    let mut defs: Vec<(usize, LocalId, LocalId, i32)> = Vec::new();
    for (base, c, at, v) in &sums {
        let t = m.locals.add(ValType::I32);
        defs.push((*at, t, *base, *c));
        for &(s, k) in v {
            repl.entry(s).or_default().push((k, t));
        }
        stats.sums += 1;
        stats.uses += v.len();
    }
    let f = m.funcs.get_mut(fid).kind.unwrap_local_mut();
    let entry = f.entry_block();
    let at = |i: Instr| (i, InstrLocId::default());
    // Each occurrence rewritten in the entry block moves the definition
    // points after it two instructions up.
    let mut entry_uses: Vec<usize> = repl
        .get(&entry)
        .map(|uses| uses.iter().map(|u| u.0).collect())
        .unwrap_or_default();
    entry_uses.sort_unstable();
    // Both edits are made in place, moving only what follows the first one.
    for (s, mut uses) in repl {
        uses.sort_unstable_by_key(|u| u.0);
        let instrs = &mut f.block_mut(s).instrs;
        let (mut read, mut write) = (uses[0].0, uses[0].0);
        let mut uses = uses.into_iter().peekable();
        while read < instrs.len() {
            if let Some((_, t)) = uses.next_if(|u| u.0 == read) {
                instrs[write] = at(LocalGet { local: t }.into());
                read += 3;
            } else {
                instrs.swap(write, read);
                read += 1;
            }
            write += 1;
        }
        instrs.truncate(write);
    }
    // Definitions at the same point go in reverse order.
    let mut defs: Vec<_> = defs
        .into_iter()
        .rev()
        .map(|(point, t, base, c)| {
            let moved = 2 * entry_uses.partition_point(|&k| k < point);
            (point - moved, (t, base, c))
        })
        .collect();
    defs.sort_by_key(|d| d.0);
    let instrs = &mut f.block_mut(entry).instrs;
    let mut read = instrs.len();
    let mut write = read + 4 * defs.len();
    instrs.resize(write, at(Unreachable {}.into()));
    for (point, (t, base, c)) in defs.into_iter().rev() {
        while read > point {
            read -= 1;
            write -= 1;
            instrs.swap(read, write);
        }
        write -= 4;
        instrs[write] = at(LocalGet { local: base }.into());
        instrs[write + 1] = at(Const {
            value: Value::I32(c),
        }
        .into());
        instrs[write + 2] = at(Binop {
            op: BinaryOp::I32Add,
        }
        .into());
        instrs[write + 3] = at(LocalSet { local: t }.into());
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
