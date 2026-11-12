//! Constant hoisting (binaryen's `const-hoisting`, which `-Oz` does not run).
//!
//! A constant used many times in one function is stored in a local once, at
//! the top of the function, and every use becomes a `local.get`:
//!
//! ```text
//! i32.const 1049517 … i32.const 1049517 … i32.const 1049517     ;; 5 bytes each
//! →
//! i32.const 1049517; local.set $c … local.get $c … local.get $c … local.get $c
//! ```
//!
//! Rust's static data starts at 1 MiB, so every address of a string, vtable or
//! panic location is a 5-byte `i32.const`; 64-bit masks and float constants
//! are up to 11 bytes. A `local.get` is 2.
//!
//! The local is written once, before anything else runs, and the function
//! never writes it again, so each `local.get` yields the constant.

use crate::passes::branches::children;
use std::collections::{HashMap, HashSet};
use walrus::ir::*;
use walrus::{LocalFunction, LocalId, Module, ValType};

#[derive(Default, Debug)]
pub struct Stats {
    pub constants: usize,
    pub uses: usize,
}

fn sleb_len(v: i64) -> usize {
    let mut n = 1;
    let mut v = v;
    while !(-64..64).contains(&v) {
        v >>= 7;
        n += 1;
    }
    n
}

/// A hashable constant: its type and bits. `None` for `v128`, which is rare
/// and has no cheap `Hash`.
fn key(v: Value) -> Option<(ValType, u64)> {
    Some(match v {
        Value::I32(x) => (ValType::I32, x as u32 as u64),
        Value::I64(x) => (ValType::I64, x as u64),
        Value::F32(x) => (ValType::F32, x.to_bits() as u64),
        Value::F64(x) => (ValType::F64, x.to_bits()),
        Value::V128(_) => return None,
    })
}

fn value((ty, bits): (ValType, u64)) -> Value {
    match ty {
        ValType::I32 => Value::I32(bits as u32 as i32),
        ValType::I64 => Value::I64(bits as i64),
        ValType::F32 => Value::F32(f32::from_bits(bits as u32)),
        _ => Value::F64(f64::from_bits(bits)),
    }
}

/// Encoded bytes of the `const` instruction.
fn size((ty, bits): (ValType, u64)) -> usize {
    match ty {
        ValType::I32 => 1 + sleb_len(bits as u32 as i32 as i64),
        ValType::I64 => 1 + sleb_len(bits as i64),
        ValType::F32 => 5,
        _ => 9,
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

/// The constants worth hoisting, in a fixed order. Reads only.
fn plan(m: &Module, f: &LocalFunction) -> Vec<(ValType, u64)> {
    let mut uses: HashMap<(ValType, u64), usize> = HashMap::new();
    // Types that already have a non-parameter local: one more of the same
    // type is free to declare, a new type costs a 2-byte group.
    let mut declared: HashSet<ValType> = HashSet::new();
    for s in seqs(f) {
        for (i, _) in &f.block(s).instrs {
            match i {
                Instr::Const(c) => {
                    if let Some(k) = key(c.value) {
                        *uses.entry(k).or_default() += 1;
                    }
                }
                Instr::LocalGet(LocalGet { local })
                | Instr::LocalSet(LocalSet { local })
                | Instr::LocalTee(LocalTee { local })
                    if !f.args.contains(local) =>
                {
                    declared.insert(m.locals.get(*local).ty());
                }
                _ => {}
            }
        }
    }
    let mut chosen: Vec<(ValType, u64)> = uses
        .into_iter()
        .filter(|&(k, n)| {
            let (n, s) = (n as i64, size(k) as i64);
            let decl = if declared.contains(&k.0) { 0 } else { 2 };
            // n constants against one constant, a `local.set` and n `local.get`s.
            n * s > s + 2 + n * 2 + decl
        })
        .map(|(k, _)| k)
        .collect();
    let rank = |ty: ValType| match ty {
        ValType::I32 => 0,
        ValType::I64 => 1,
        ValType::F32 => 2,
        _ => 3,
    };
    chosen.sort_by_key(|&(ty, bits)| (rank(ty), bits));
    chosen
}

fn apply(m: &mut Module, fid: walrus::FunctionId, consts: Vec<(ValType, u64)>, stats: &mut Stats) {
    let locals: HashMap<(ValType, u64), LocalId> =
        consts.iter().map(|&k| (k, m.locals.add(k.0))).collect();
    let f = m.funcs.get_mut(fid).kind.unwrap_local_mut();
    for s in seqs(f) {
        for (i, _) in &mut f.block_mut(s).instrs {
            if let Instr::Const(c) = i
                && let Some(k) = key(c.value)
                && let Some(&local) = locals.get(&k)
            {
                *i = LocalGet { local }.into();
                stats.uses += 1;
            }
        }
    }
    let entry = f.entry_block();
    let defs = consts.iter().flat_map(|&k| {
        [
            (Const { value: value(k) }.into(), InstrLocId::default()),
            (LocalSet { local: locals[&k] }.into(), InstrLocId::default()),
        ]
    });
    f.block_mut(entry).instrs.splice(0..0, defs);
    stats.constants += consts.len();
}

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    // Plan in parallel; apply in function order, since new locals must be
    // allocated in the same order every run.
    for (id, consts) in crate::util::par_map(m, |_, f| plan(m, f)) {
        if !consts.is_empty() {
            apply(m, id, consts, &mut stats);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use crate::test_util::{flat, run};

    fn check(body: &str, want: &str) {
        let wat = format!("(module (func $f (param i32) (result i32) {body}))");
        assert_eq!(
            run(&wat, |m| {
                super::run(m);
            }),
            flat(want)
        );
    }

    #[test]
    fn hoists_a_large_constant_used_often() {
        // 4 × 5 bytes against 5 + 2 + 4 × 2, plus 2 to declare the local.
        check(
            "i32.const 1048576  i32.const 1048576  i32.add
             i32.const 1048576  i32.add  i32.const 1048576  i32.add",
            "(local i32)
             i32.const 1048576  local.set 1
             local.get 1  local.get 1  i32.add
             local.get 1  i32.add  local.get 1  i32.add",
        );
    }

    #[test]
    fn leaves_a_constant_used_too_rarely() {
        // 3 × 5 bytes is no more than 5 + 2 + 3 × 2 + 2.
        let body = "i32.const 1048576  i32.const 1048576  i32.add  i32.const 1048576  i32.add";
        check(body, body);
    }

    #[test]
    fn leaves_small_constants() {
        // A one-byte constant is no bigger than a `local.get`.
        let body = "i32.const 7  i32.const 7  i32.add  i32.const 7  i32.add
                    i32.const 7  i32.add  i32.const 7  i32.add  i32.const 7  i32.add";
        check(body, body);
    }

    #[test]
    fn hoists_each_type_separately() {
        check(
            "f64.const 1.5  f64.const 1.5  f64.add  f64.const 1.5  f64.add  i64.trunc_sat_f64_s
             i64.const 1500000000  i64.add  i64.const 1500000000  i64.add  i64.const 1500000000  i64.add
             i64.const 1500000000  i64.add  i32.wrap_i64",
            "(local i64 f64)
             i64.const 1500000000  local.set 1
             f64.const 0x1.8p+0  local.set 2
             local.get 2  local.get 2  f64.add  local.get 2  f64.add  i64.trunc_sat_f64_s
             local.get 1  i64.add  local.get 1  i64.add  local.get 1  i64.add
             local.get 1  i64.add  i32.wrap_i64",
        );
    }

    #[test]
    fn the_definition_runs_before_a_use_in_a_loop() {
        check(
            "loop (result i32)
               i32.const 1048576  i32.const 1048576  i32.add
               i32.const 1048576  i32.add  i32.const 1048576  i32.add
               local.get 0  br_if 0
             end",
            "(local i32)
             i32.const 1048576  local.set 1
             loop (result i32)
               local.get 1  local.get 1  i32.add
               local.get 1  i32.add  local.get 1  i32.add
               local.get 0  br_if 0
             end",
        );
    }
}
