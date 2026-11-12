//! Encoded-size estimates computed from walrus IR, without emitting.
//!
//! Exact sizes depend on the final function, local and label numbering, which
//! only emission decides. These estimates are close enough to rank inlining
//! decisions: indices are assumed to take `call_leb` bytes for functions and one
//! byte for locals and labels.

use crate::util::leb_len;
use std::collections::HashSet;
use walrus::ir::*;
use walrus::{LocalFunction, Module, ValType};

fn sleb_len(v: i64) -> usize {
    let mut n = 1;
    let mut v = v;
    while !(-64..64).contains(&v) {
        v >>= 7;
        n += 1;
    }
    n
}

fn memarg(m: &MemArg) -> usize {
    1 + leb_len(m.offset)
}

/// Bytes of one instruction, not counting the contents of nested sequences.
/// Structured instructions include their `end` (and a non-empty `else`).
pub fn instr(f: &LocalFunction, i: &Instr, call_leb: usize) -> usize {
    match i {
        Instr::Block(_) | Instr::Loop(_) => 3,
        Instr::IfElse(e) => 3 + usize::from(!f.block(e.alternative).instrs.is_empty()),
        Instr::TryTable(t) => 3 + leb_len(t.catches.len() as u64) + 3 * t.catches.len(),
        Instr::Try(t) => 3 + 2 * t.catches.len(),
        Instr::Const(c) => match c.value {
            Value::I32(v) => 1 + sleb_len(v as i64),
            Value::I64(v) => 1 + sleb_len(v),
            Value::F32(_) => 5,
            Value::F64(_) => 9,
            Value::V128(_) => 18,
        },
        Instr::Call(_) | Instr::ReturnCall(_) | Instr::RefFunc(_) => 1 + call_leb,
        Instr::CallIndirect(_) | Instr::ReturnCallIndirect(_) => 3,
        Instr::LocalGet(_)
        | Instr::LocalSet(_)
        | Instr::LocalTee(_)
        | Instr::GlobalGet(_)
        | Instr::GlobalSet(_)
        | Instr::Br(_)
        | Instr::BrIf(_) => 2,
        Instr::BrTable(b) => 2 + leb_len(b.blocks.len() as u64) + b.blocks.len(),
        Instr::Load(l) => 1 + memarg(&l.arg),
        Instr::Store(s) => 1 + memarg(&s.arg),
        Instr::MemorySize(_) | Instr::MemoryGrow(_) => 2,
        Instr::MemoryCopy(_) => 4,
        Instr::MemoryFill(_) | Instr::MemoryInit(_) | Instr::DataDrop(_) => 3,
        Instr::Unop(u) => match u.op {
            // The saturating truncations and sign extensions of
            // `nontrapping-float-to-int` are 0xfc-prefixed.
            UnaryOp::I32TruncSSatF32
            | UnaryOp::I32TruncUSatF32
            | UnaryOp::I32TruncSSatF64
            | UnaryOp::I32TruncUSatF64
            | UnaryOp::I64TruncSSatF32
            | UnaryOp::I64TruncUSatF32
            | UnaryOp::I64TruncSSatF64
            | UnaryOp::I64TruncUSatF64 => 2,
            _ => 1,
        },
        Instr::Throw(_) | Instr::Rethrow(_) | Instr::RefNull(_) => 2,
        Instr::TableGet(_) | Instr::TableSet(_) => 2,
        Instr::TableGrow(_) | Instr::TableSize(_) | Instr::TableFill(_) => 3,
        _ => 1,
    }
}

/// (instruction bytes, whole body bytes) of a function: the body adds the
/// local declarations and the final `end`.
pub fn function(m: &Module, f: &LocalFunction, call_leb: usize) -> (usize, usize) {
    let args: HashSet<_> = f.args.iter().copied().collect();
    let mut types: HashSet<ValType> = HashSet::new();
    let mut bytes = 0;
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        for (i, _) in &f.block(s).instrs {
            bytes += instr(f, i, call_leb);
            match i {
                Instr::LocalGet(LocalGet { local })
                | Instr::LocalSet(LocalSet { local })
                | Instr::LocalTee(LocalTee { local })
                    if !args.contains(local) =>
                {
                    types.insert(m.locals.get(*local).ty());
                }
                _ => {}
            }
            stack.extend(crate::passes::inline::children(i));
        }
    }
    // One (count, type) group per local type, plus the group count.
    let decl = 1 + 2 * types.len();
    (bytes, bytes + decl + 1)
}
