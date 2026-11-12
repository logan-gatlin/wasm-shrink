//! Forwarding-thunk elimination.
//!
//! A function whose whole body is `local.get 0 .. local.get n-1; call $g`, with
//! the same type as `$g`, is a pure alias of `$g`. Every reference to it
//! (including table slots and exports, which inlining cannot touch) is pointed at
//! `$g` and the thunk becomes dead. Rust emits these for trait-object shims and
//! `#[inline(never)]` wrappers that LLVM reduced to a tail forward.

use crate::util::replace_functions;
use std::collections::HashMap;
use walrus::ir::Instr;
use walrus::{FunctionId, FunctionKind, Module};

#[derive(Default, Debug)]
pub struct Stats {
    #[allow(dead_code)] // reported through Debug
    pub removed: usize,
}

fn target(m: &Module, id: FunctionId) -> Option<FunctionId> {
    let FunctionKind::Local(f) = &m.funcs.get(id).kind else {
        return None;
    };
    let body = &f.block(f.entry_block()).instrs;
    let (last, gets) = body.split_last()?;
    let Instr::Call(call) = &last.0 else {
        return None;
    };
    if gets.len() != f.args.len() || call.func == id {
        return None;
    }
    for ((instr, _), arg) in gets.iter().zip(&f.args) {
        match instr {
            Instr::LocalGet(g) if g.local == *arg => {}
            _ => return None,
        }
    }
    let callee_ty = m.funcs.get(call.func).ty();
    if m.types.get(callee_ty) != m.types.get(f.ty()) {
        return None;
    }
    Some(call.func)
}

pub fn run(m: &mut Module) -> Stats {
    let mut map: HashMap<FunctionId, FunctionId> = HashMap::new();
    let ids: Vec<FunctionId> = m.funcs.iter_local().map(|(id, _)| id).collect();
    for id in ids {
        if let Some(t) = target(m, id) {
            map.insert(id, t);
        }
    }
    // Break cycles (a -> b -> a): drop one edge of each.
    let keys: Vec<_> = map.keys().copied().collect();
    for k in keys {
        let mut seen = vec![k];
        let mut cur = k;
        while let Some(&n) = map.get(&cur) {
            if seen.contains(&n) {
                map.remove(&cur);
                break;
            }
            seen.push(n);
            cur = n;
        }
    }
    replace_functions(m, &map);
    for &t in map.keys() {
        m.funcs.delete(t);
    }
    Stats { removed: map.len() }
}
