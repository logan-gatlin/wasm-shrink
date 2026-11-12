//! Branch cleanups (the size-relevant part of binaryen's `remove-unused-brs`
//! and `merge-blocks`).
//!
//! - `block $b  <cond>  br_if $b  <rest>  end` → `<cond'> if <rest> end`, where
//!   `<cond'>` is the condition inverted for free when it ends in a comparison or
//!   `i32.eqz`, otherwise followed by `i32.eqz`. LLVM emits this shape for every
//!   early exit; it costs 5 bytes of control flow, the `if` costs 3 (or 4).
//! - A `br` to the end of its own block, as the block's last instruction, is
//!   removed when the stack already holds exactly the block's results.
//! - `block $b … end  br $x`: a branch to `$b` only goes on to `$x`, so it
//!   branches to `$x` directly. Likewise a `br $b` becomes `unreachable` or
//!   `return` when that is what follows `$b`. `fold-tails` leaves many of
//!   these; the block usually ends up with no branches to it and is unwrapped.
//! - Instructions after one that never falls through are removed.
//! - `block $b … <cond> if <A> br $b end <B> end` → `… <cond> if <A> else <B> end`:
//!   the rest of the block becomes the `else` arm, and the `br` is not needed.
//! - A block nothing branches to is unwrapped into its parent.

use crate::stack::effect_with_br_if;
use std::collections::{HashMap, HashSet};
use walrus::ir::*;
use walrus::{LocalFunction, Module};

#[derive(Default, Debug)]
pub struct Stats {
    pub br_if_to_if: usize,
    pub inverted: usize,
    pub threaded: usize,
    pub trailing_br: usize,
    pub if_else: usize,
    pub unwrapped: usize,
}

fn targets(i: &Instr, out: &mut Vec<InstrSeqId>) {
    match i {
        Instr::Br(b) => out.push(b.block),
        Instr::BrIf(b) => out.push(b.block),
        Instr::BrTable(b) => out.extend(b.blocks.iter().copied().chain([b.default])),
        Instr::BrOnNull(b) => out.push(b.block),
        Instr::BrOnNonNull(b) => out.push(b.block),
        Instr::BrOnCast(b) => out.push(b.block),
        Instr::BrOnCastFail(b) => out.push(b.block),
        Instr::TryTable(t) => {
            for c in &t.catches {
                match c {
                    TryTableCatch::Catch { label, .. }
                    | TryTableCatch::CatchRef { label, .. }
                    | TryTableCatch::CatchAll { label }
                    | TryTableCatch::CatchAllRef { label } => out.push(*label),
                }
            }
        }
        Instr::Try(t) => {
            // Legacy `delegate`/`rethrow` use depths, not ids; treat a legacy
            // try as pinning nothing but never restructure around it (see `run`).
            let _ = t;
        }
        _ => {}
    }
}

pub(crate) fn children(i: &Instr) -> Vec<InstrSeqId> {
    match i {
        Instr::Block(b) => vec![b.seq],
        Instr::Loop(l) => vec![l.seq],
        Instr::IfElse(e) => vec![e.consequent, e.alternative],
        Instr::TryTable(t) => vec![t.seq],
        Instr::Try(t) => {
            let mut v = vec![t.seq];
            for c in &t.catches {
                if let LegacyCatch::Catch { handler, .. } | LegacyCatch::CatchAll { handler } = c {
                    v.push(*handler);
                }
            }
            v
        }
        _ => vec![],
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

fn has_legacy_eh(f: &LocalFunction) -> bool {
    seqs(f).iter().any(|&s| {
        f.block(s)
            .instrs
            .iter()
            .any(|(i, _)| matches!(i, Instr::Try(_) | Instr::Rethrow(_)))
    })
}

/// Stack height after `instrs`, or `None` if it cannot be tracked.
fn height(
    m: &Module,
    f: &LocalFunction,
    loops: &HashSet<InstrSeqId>,
    instrs: &[(Instr, InstrLocId)],
) -> Option<usize> {
    let mut h = 0usize;
    for (i, _) in instrs {
        let (pop, push) = effect_with_br_if(m, f, loops, i)?;
        h = h.checked_sub(pop)? + push;
    }
    Some(h)
}

/// The inverse of a comparison, if it is free.
fn invert(op: BinaryOp) -> Option<BinaryOp> {
    use BinaryOp::*;
    Some(match op {
        I32Eq => I32Ne,
        I32Ne => I32Eq,
        I32LtS => I32GeS,
        I32GeS => I32LtS,
        I32LtU => I32GeU,
        I32GeU => I32LtU,
        I32GtS => I32LeS,
        I32LeS => I32GtS,
        I32GtU => I32LeU,
        I32LeU => I32GtU,
        I64Eq => I64Ne,
        I64Ne => I64Eq,
        I64LtS => I64GeS,
        I64GeS => I64LtS,
        I64LtU => I64GeU,
        I64GeU => I64LtU,
        I64GtS => I64LeS,
        I64LeS => I64GtS,
        I64GtU => I64LeU,
        I64LeU => I64GtU,
        _ => return None,
    })
}

fn result_arity(m: &Module, f: &LocalFunction, s: InstrSeqId) -> usize {
    match f.block(s).ty {
        InstrSeqType::Simple(None) => 0,
        InstrSeqType::Simple(Some(_)) => 1,
        InstrSeqType::MultiValue(t) => m.types.get(t).results().len(),
    }
}

fn function(m: &Module, f: &mut LocalFunction, stats: &mut Stats) -> bool {
    let mut changed = false;
    // Loop bodies are branch targets at their start: a `br` there continues.
    let is_loop = crate::stack::loops(f);
    let entry = f.entry_block();
    let no_results = m.types.get(f.ty()).results().is_empty();
    // Jump threading. `next[b]` is what a branch to block `b` does next.
    enum Next {
        Br(InstrSeqId),
        Unreachable,
        Return,
    }
    let mut next: HashMap<InstrSeqId, Next> = HashMap::new();
    for s in seqs(f) {
        for w in f.block(s).instrs.windows(2) {
            let Instr::Block(Block { seq: b }) = w[0].0 else {
                continue;
            };
            if !matches!(f.block(b).ty, InstrSeqType::Simple(None)) {
                continue;
            }
            let n = match &w[1].0 {
                // The branch must carry nothing, as branches to `b` do.
                Instr::Br(x)
                    if x.block != b && crate::stack::label_arity(m, f, &is_loop, x.block) == 0 =>
                {
                    Next::Br(x.block)
                }
                Instr::Unreachable(_) => Next::Unreachable,
                Instr::Return(_) if no_results => Next::Return,
                _ => continue,
            };
            next.insert(b, n);
        }
    }
    if !next.is_empty() {
        // Follow chains: each step leads to an enclosing label, so this ends.
        let resolve = |mut b: InstrSeqId| {
            while let Some(Next::Br(x)) = next.get(&b) {
                b = *x;
            }
            b
        };
        for s in seqs(f) {
            for (i, _) in &mut f.block_mut(s).instrs {
                let before = stats.threaded;
                let mut thread = |b: &mut InstrSeqId| {
                    let to = resolve(*b);
                    if to != *b {
                        *b = to;
                        stats.threaded += 1;
                    }
                };
                match i {
                    Instr::Br(b) => {
                        thread(&mut b.block);
                        match next.get(&b.block) {
                            Some(Next::Unreachable) => {
                                *i = Unreachable {}.into();
                                stats.threaded += 1;
                            }
                            Some(Next::Return) => {
                                *i = Return {}.into();
                                stats.threaded += 1;
                            }
                            _ => {}
                        }
                    }
                    Instr::BrIf(b) => thread(&mut b.block),
                    Instr::BrTable(t) => {
                        t.blocks.iter_mut().for_each(&mut thread);
                        thread(&mut t.default);
                    }
                    _ => {}
                }
                changed |= stats.threaded != before;
            }
        }
    }
    // Code after an instruction that never falls through, such as the `br`
    // left behind a block once its branches are threaded past it.
    crate::passes::tails::dce_function(f);
    // A `br` to the end of its own block, as the block's last instruction:
    // not needed when the stack already holds exactly the block's results.
    for s in seqs(f) {
        let instrs = &f.block(s).instrs;
        let n = instrs.len();
        if !is_loop.contains(&s)
            && matches!(instrs.last(), Some((Instr::Br(b), _)) if b.block == s)
            && height(m, f, &is_loop, &instrs[..n - 1]) == Some(result_arity(m, f, s))
        {
            f.block_mut(s).instrs.pop();
            stats.trailing_br += 1;
            changed = true;
        }
    }
    // `block $b … if <A> br $b end <B> end`: `<B>` becomes the `else` arm
    // (an empty one when the `if` is the last thing in the block).
    for b in seqs(f) {
        if is_loop.contains(&b)
            || b == entry
            || !matches!(f.block(b).ty, InstrSeqType::Simple(None))
        {
            continue;
        }
        let instrs = &f.block(b).instrs;
        let Some((k, then, alt)) = instrs.iter().enumerate().find_map(|(k, (i, _))| match i {
            Instr::IfElse(e)
                if f.block(e.alternative).instrs.is_empty()
                    && matches!(f.block(e.consequent).ty, InstrSeqType::Simple(None))
                    && matches!(f.block(e.consequent).instrs.last(), Some((Instr::Br(x), _)) if x.block == b) =>
            {
                Some((k, e.consequent, e.alternative))
            }
            _ => None,
        }) else {
            continue;
        };
        // The `else` arm starts with an empty stack, so `<B>` must not use
        // anything pushed before the `if`: either nothing is there, or `<B>`
        // is balanced on its own.
        let then_len = f.block(then).instrs.len();
        let ok = (height(m, f, &is_loop, &instrs[..k]) == Some(1)
            || height(m, f, &is_loop, &instrs[k + 1..]) == Some(0))
            && height(m, f, &is_loop, &f.block(then).instrs[..then_len - 1]) == Some(0);
        if !ok {
            continue;
        }
        let rest = f.block_mut(b).instrs.split_off(k + 1);
        f.block_mut(alt).instrs = rest;
        f.block_mut(then).instrs.pop();
        stats.if_else += 1;
        changed = true;
    }
    let mut refs: HashMap<InstrSeqId, usize> = HashMap::new();
    for s in seqs(f) {
        let mut t = Vec::new();
        for (i, _) in &f.block(s).instrs {
            targets(i, &mut t);
        }
        for x in t {
            *refs.entry(x).or_default() += 1;
        }
    }
    for s in seqs(f) {
        let mut j = 0;
        while j < f.block(s).instrs.len() {
            let Instr::Block(Block { seq: b }) = f.block(s).instrs[j].0 else {
                j += 1;
                continue;
            };
            let r = refs.get(&b).copied().unwrap_or(0);
            if r == 0 {
                // Nothing branches here: splice the body in place of the block.
                let body = std::mem::take(&mut f.block_mut(b).instrs);
                let n = body.len();
                f.block_mut(s).instrs.splice(j..j + 1, body);
                stats.unwrapped += 1;
                changed = true;
                let _ = n;
                continue; // re-examine the spliced instructions
            }
            if r == 1 && matches!(f.block(b).ty, InstrSeqType::Simple(None)) {
                let k = f
                    .block(b)
                    .instrs
                    .iter()
                    .position(|(i, _)| matches!(i, Instr::BrIf(x) if x.block == b));
                if let Some(k) = k
                    && height(m, f, &is_loop, &f.block(b).instrs[..k]) == Some(1)
                {
                    let mut body = std::mem::take(&mut f.block_mut(b).instrs);
                    let rest = body.split_off(k + 1);
                    body.pop(); // the br_if
                    let inverted = match body.last_mut() {
                        Some((Instr::Binop(x), _)) => match invert(x.op) {
                            Some(op) => {
                                x.op = op;
                                true
                            }
                            None => false,
                        },
                        Some((
                            Instr::Unop(Unop {
                                op: UnaryOp::I32Eqz,
                            }),
                            _,
                        )) => {
                            body.pop();
                            true
                        }
                        _ => false,
                    };
                    if inverted {
                        stats.inverted += 1;
                    } else {
                        body.push((
                            Unop {
                                op: UnaryOp::I32Eqz,
                            }
                            .into(),
                            InstrLocId::default(),
                        ));
                    }
                    f.block_mut(b).instrs = rest;
                    let alt = f
                        .builder_mut()
                        .dangling_instr_seq(InstrSeqType::Simple(None))
                        .id();
                    body.push((
                        IfElse {
                            consequent: b,
                            alternative: alt,
                        }
                        .into(),
                        InstrLocId::default(),
                    ));
                    let n = body.len();
                    f.block_mut(s).instrs.splice(j..j + 1, body);
                    stats.br_if_to_if += 1;
                    changed = true;
                    j += n;
                    continue;
                }
            }
            j += 1;
        }
    }
    changed
}

crate::sum_stats!(Stats: br_if_to_if, inverted, threaded, trailing_br, if_else, unwrapped);

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    // Each function is taken out so `m` can be read for types while it changes.
    for (_, s) in crate::util::par_take(m, |m, _, f| {
        let mut stats = Stats::default();
        if !has_legacy_eh(f) {
            for _ in 0..4 {
                if !function(m, f, &mut stats) {
                    break;
                }
            }
        }
        stats
    }) {
        stats += s;
    }
    stats
}

#[cfg(test)]
mod tests {
    use crate::test_util::{flat, run};

    const PRELUDE: &str = "(func $g (param i32))";

    fn check(body: &str, want: &str) {
        let wat = format!("(module {PRELUDE} (func $f (param i32 i32) {body}))");
        assert_eq!(
            run(&wat, |m| {
                super::run(m);
            }),
            flat(want)
        );
    }

    #[test]
    fn br_if_out_of_a_block_becomes_if() {
        check(
            "block  local.get 0  br_if 0  i32.const 1  call $g  end",
            "local.get 0  i32.eqz  if  i32.const 1  call $g  end",
        );
    }

    #[test]
    fn threads_a_branch_through_a_block_followed_by_br() {
        // `br_if 0` only reaches `br 1`; it can go there itself, and the
        // inner block is then not a target of anything.
        check(
            "loop
               block
                 local.get 0  br_if 0
                 local.get 1  br_if 0
                 i32.const 1  call $g
                 return
               end
               br 0
             end",
            "loop
               local.get 0  br_if 0
               local.get 1  br_if 0
               i32.const 1  call $g
               return
             end",
        );
    }

    #[test]
    fn threads_a_br_into_unreachable() {
        check(
            "block
               local.get 0  if  i32.const 1  call $g  br 1  end
               i32.const 2  call $g
               return
             end
             unreachable",
            "local.get 0  if  i32.const 1  call $g  unreachable  end
             i32.const 2  call $g
             return",
        );
    }

    #[test]
    fn does_not_thread_into_a_branch_that_carries_a_value() {
        // `br 0` after the inner block carries the 2 to the outer one; a
        // branch from inside the inner block has no such value to carry.
        let wat = "(module (func $f (param i32 i32) (result i32)
             block (result i32)
               i32.const 2
               block
                 local.get 0  br_if 0
                 i32.const 1  return
               end
               br 0
             end))";
        assert_eq!(
            run(wat, |m| {
                super::run(m);
            }),
            flat("i32.const 2  local.get 0  i32.eqz  if  i32.const 1  return  end")
        );
    }

    #[test]
    fn removes_a_br_from_an_if_that_ends_its_target() {
        // Nothing follows the `if`, so the `else` arm is empty.
        check(
            "block
               local.get 0  br_if 0
               local.get 1  if  i32.const 1  call $g  br 1  end
             end
             i32.const 2  call $g",
            "local.get 0  i32.eqz
             if  local.get 1  if  i32.const 1  call $g  end  end
             i32.const 2  call $g",
        );
    }

    #[test]
    fn the_else_arm_keeps_its_branches() {
        check(
            "block
               local.get 0  if  i32.const 1  call $g  br 1  end
               local.get 1  br_if 0
               i32.const 3  call $g
             end
             i32.const 2  call $g",
            "block
               local.get 0
               if  i32.const 1  call $g
               else  local.get 1  br_if 1  i32.const 3  call $g
               end
             end
             i32.const 2  call $g",
        );
    }

    #[test]
    fn the_rest_of_a_block_becomes_the_else_arm() {
        check(
            "block
               local.get 0  if  i32.const 1  call $g  br 1  end
               i32.const 3  call $g
             end
             i32.const 2  call $g",
            "local.get 0
             if  i32.const 1  call $g
             else  i32.const 3  call $g
             end
             i32.const 2  call $g",
        );
    }

    #[test]
    fn no_else_arm_when_the_rest_uses_the_stack_below() {
        // `call $g` after the `if` pops the 5 pushed before it.
        let body = "block
               i32.const 5
               local.get 0  if  i32.const 1  call $g  br 1  end
               call $g
             end
             i32.const 2  call $g";
        check(body, body);
    }
}
