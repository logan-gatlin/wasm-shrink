//! Stack effects of walrus instructions, for passes that move instructions
//! across block boundaries and need to know the operand-stack height.

use std::collections::HashSet;
use walrus::ir::*;
use walrus::{LocalFunction, Module};

/// The loop bodies of `f`.
pub fn loops(f: &LocalFunction) -> HashSet<InstrSeqId> {
    let mut out = HashSet::new();
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        for (i, _) in &f.block(s).instrs {
            if let Instr::Loop(l) = i {
                out.insert(l.seq);
            }
            stack.extend(crate::passes::branches::children(i));
        }
    }
    out
}

/// How many values a branch to `s` carries: a loop's parameters (the branch
/// goes back to its start), any other sequence's results.
pub fn label_arity(
    m: &Module,
    f: &LocalFunction,
    loops: &HashSet<InstrSeqId>,
    s: InstrSeqId,
) -> usize {
    match f.block(s).ty {
        InstrSeqType::Simple(None) => 0,
        InstrSeqType::Simple(Some(_)) => usize::from(!loops.contains(&s)),
        InstrSeqType::MultiValue(t) => {
            let t = m.types.get(t);
            if loops.contains(&s) {
                t.params().len()
            } else {
                t.results().len()
            }
        }
    }
}

/// [`effect`], extended to `br_if`: it pops its condition and leaves the
/// values it would carry where they are. `loops` is [`loops`]`(f)`.
pub fn effect_with_br_if(
    m: &Module,
    f: &LocalFunction,
    loops: &HashSet<InstrSeqId>,
    i: &Instr,
) -> Option<(usize, usize)> {
    match i {
        Instr::BrIf(b) => {
            let n = label_arity(m, f, loops, b.block);
            Some((n + 1, n))
        }
        _ => effect(m, f, i),
    }
}

/// (values popped, values pushed), or `None` when the instruction is
/// stack-polymorphic (it never falls through) or not modelled here.
pub fn effect(m: &Module, f: &LocalFunction, i: &Instr) -> Option<(usize, usize)> {
    let seq_arity = |s: InstrSeqId| match f.block(s).ty {
        InstrSeqType::Simple(None) => (0, 0),
        InstrSeqType::Simple(Some(_)) => (0, 1),
        InstrSeqType::MultiValue(t) => {
            let t = m.types.get(t);
            (t.params().len(), t.results().len())
        }
    };
    let fn_ty = |t: walrus::TypeId| {
        let t = m.types.get(t);
        (t.params().len(), t.results().len())
    };
    Some(match i {
        Instr::Block(b) => seq_arity(b.seq),
        Instr::Loop(l) => seq_arity(l.seq),
        Instr::IfElse(e) => {
            let (p, r) = seq_arity(e.consequent);
            (p + 1, r)
        }
        Instr::TryTable(t) => seq_arity(t.seq),
        Instr::Call(c) => fn_ty(m.funcs.get(c.func).ty()),
        Instr::CallIndirect(c) => {
            let (p, r) = fn_ty(c.ty);
            (p + 1, r)
        }
        Instr::CallRef(c) => {
            let (p, r) = fn_ty(c.ty);
            (p + 1, r)
        }
        Instr::LocalGet(_) | Instr::GlobalGet(_) | Instr::Const(_) => (0, 1),
        Instr::LocalSet(_) | Instr::GlobalSet(_) | Instr::Drop(_) => (1, 0),
        Instr::LocalTee(_)
        | Instr::Unop(_)
        | Instr::Load(_)
        | Instr::LoadSimd(_)
        | Instr::MemoryGrow(_) => (1, 1),
        Instr::Binop(_) => (2, 1),
        Instr::TernOp(_) | Instr::Select(_) | Instr::Cmpxchg(_) | Instr::AtomicWait(_) => (3, 1),
        Instr::Store(_) | Instr::TableSet(_) => (2, 0),
        Instr::AtomicRmw(_) | Instr::AtomicNotify(_) | Instr::TableGrow(_) => (2, 1),
        Instr::MemoryInit(_)
        | Instr::MemoryCopy(_)
        | Instr::MemoryFill(_)
        | Instr::TableFill(_) => (3, 0),
        Instr::TableInit(_) | Instr::TableCopy(_) => (3, 0),
        Instr::MemorySize(_) | Instr::TableSize(_) | Instr::RefNull(_) | Instr::RefFunc(_) => {
            (0, 1)
        }
        Instr::TableGet(_) | Instr::RefIsNull(_) | Instr::RefAsNonNull(_) => (1, 1),
        Instr::DataDrop(_) | Instr::ElemDrop(_) | Instr::AtomicFence(_) => (0, 0),
        _ => return None,
    })
}
