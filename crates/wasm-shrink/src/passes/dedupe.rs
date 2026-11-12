//! Duplicate function elimination.
//!
//! Two functions with the same type and byte-identical encoded bodies behave
//! the same, so every reference to one can point at the other. Bodies are
//! compared in walrus's own encoding (locals become indices, labels become
//! depths), so no hand-written structural hash can drift from the IR.
//!
//! Call targets are part of the bytes, so merging one round can make callers
//! identical; the pass repeats until nothing changes. This is binaryen's
//! `duplicate-function-elimination`.
//!
//! The module is encoded once. Each body is split into its bytes with every
//! function-index immediate cut out, plus the list of functions it refers to.
//! A merge only changes which functions bodies refer to, so later rounds
//! compare the cut bytes and the referenced functions resolved through the
//! merges so far. That finds exactly what re-encoding after every round would.

use crate::util::{encode_functions, replace_functions};
use rayon::prelude::*;
use std::collections::HashMap;
use walrus::{FunctionId, Module};
use wasmparser::{BinaryReader, FunctionBody, Operator};

#[derive(Default, Debug)]
pub struct Stats {
    pub removed: usize,
    pub rounds: usize,
}

/// A body with its function references cut out, and those references in order.
struct Cut {
    ty: u32,
    bytes: Vec<u8>,
    refs: Vec<FunctionId>,
}

fn cut(ty: u32, body: &[u8], by_index: &HashMap<u32, FunctionId>) -> Option<Cut> {
    let fb = FunctionBody::new(BinaryReader::new(body, 0));
    let mut ops = fb.get_operators_reader().ok()?;
    let start = ops.original_position();
    let mut bytes = body[..start].to_vec();
    let mut refs = Vec::new();
    while !ops.eof() {
        let (op, at) = ops.read_with_offset().ok()?;
        let end = ops.original_position();
        match op {
            Operator::Call { function_index }
            | Operator::ReturnCall { function_index }
            | Operator::RefFunc { function_index } => {
                bytes.push(body[at]); // the one-byte opcode
                refs.push(*by_index.get(&function_index)?);
            }
            _ => bytes.extend_from_slice(&body[at..end]),
        }
    }
    Some(Cut { ty, bytes, refs })
}

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    let enc = encode_functions(m);
    let by_index: HashMap<u32, FunctionId> = enc.index.iter().map(|(&id, &i)| (i, id)).collect();
    // Keep the function that sorts first so the result is deterministic.
    let mut ids: Vec<FunctionId> = enc.bodies.keys().copied().collect();
    ids.sort();
    // A body that cannot be cut is never merged.
    let cuts: Vec<(FunctionId, Cut)> = ids
        .into_par_iter()
        .map(|id| {
            let (ty, body) = &enc.bodies[&id];
            (id, cut(*ty, body, &by_index))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|(id, c)| Some((id, c?)))
        .collect();

    // dup -> kept function, accumulated over all rounds.
    let mut map: HashMap<FunctionId, FunctionId> = HashMap::new();
    loop {
        stats.rounds += 1;
        let resolve = |mut f: FunctionId| {
            while let Some(&g) = map.get(&f) {
                f = g;
            }
            f
        };
        let mut first: HashMap<(u32, &[u8], Vec<FunctionId>), FunctionId> = HashMap::new();
        let mut merged = Vec::new();
        for (id, c) in &cuts {
            if map.contains_key(id) {
                continue;
            }
            let refs: Vec<FunctionId> = c.refs.iter().map(|&f| resolve(f)).collect();
            match first.get(&(c.ty, c.bytes.as_slice(), refs.clone())) {
                Some(&keep) => merged.push((*id, keep)),
                None => {
                    first.insert((c.ty, c.bytes.as_slice(), refs), *id);
                }
            }
        }
        if merged.is_empty() || stats.rounds > 20 {
            break;
        }
        stats.removed += merged.len();
        map.extend(merged);
    }
    replace_functions(m, &map);
    for &dup in map.keys() {
        m.funcs.delete(dup);
    }
    stats
}
