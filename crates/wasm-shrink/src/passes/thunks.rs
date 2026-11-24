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
    // The same type id, not an equal signature: with GC, equal signatures in
    // different rec groups are different types to `call_indirect`.
    (m.funcs.get(call.func).ty() == f.ty()).then_some(call.func)
}

pub fn run(m: &mut Module) -> Stats {
    let ids: Vec<FunctionId> = m.funcs.iter_local().map(|(id, _)| id).collect();
    let mut edges: HashMap<FunctionId, FunctionId> = ids
        .iter()
        .filter_map(|&id| Some((id, target(m, id)?)))
        .collect();
    // Follow each chain to its end once. Taking an edge removes it, so a
    // cycle (a -> b -> a) stops at the function it started from, which stays.
    let mut map = HashMap::new();
    let mut path = Vec::new();
    for id in ids {
        let mut cur = id;
        while let Some(next) = edges.remove(&cur) {
            path.push(cur);
            cur = next;
        }
        let end = map.get(&cur).copied().unwrap_or(cur);
        for source in path.drain(..) {
            if source != end {
                map.insert(source, end);
            }
        }
    }
    replace_functions(m, &map);
    for &t in map.keys() {
        m.funcs.delete(t);
    }
    Stats { removed: map.len() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn breaks_a_forwarding_cycle_the_same_way_every_time() {
        let wasm = wat::parse_str(
            "(module
               (func $a (export \"a\") (param i32) local.get 0 call $b)
               (func $b (export \"b\") (param i32) local.get 0 call $c)
               (func $c (export \"c\") (param i32) local.get 0 call $a))",
        )
        .unwrap();
        let once = |_| {
            let mut m = walrus::Module::from_buffer(&wasm).unwrap();
            assert_eq!(super::run(&mut m).removed, 2);
            crate::test_util::emit(&mut m)
        };
        let first = once(0);
        for i in 1..20 {
            assert_eq!(once(i), first);
        }
    }
}
