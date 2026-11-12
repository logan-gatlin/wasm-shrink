//! Merge similar functions (binaryen's `merge-similar-functions`, constants only).
//!
//! Monomorphization gives Rust many copies of one generic function that differ
//! only in constants: sizes, alignments, vtable or string addresses. Functions
//! with the same type whose encoded bodies are identical except for constant
//! immediates are merged:
//!
//!   shared(params…, c0, c1, …)  = the body, with each differing constant
//!                                 replaced by a read of a new parameter
//!   original(params…)           = local.get params…; const c0; …; call shared
//!
//! The originals keep their identity (exports, tables, names), and a later
//! `inline` round folds single-caller thunks into their callers. The shared
//! function gets no name, so the name section does not grow.

use crate::passes::inline::{Snapshot, children, copy_seq, inlinable};
use crate::util::{encode_functions, leb_len};
use rayon::prelude::*;
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{FunctionBuilder, FunctionId, LocalId, Module, ValType};
use wasmparser::{BinaryReader, FunctionBody, Operator};

#[derive(Default, Debug)]
pub struct Stats {
    pub groups: usize,
    pub merged: usize,
    pub params_added: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum K {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128(u128),
}

impl K {
    fn ty(&self) -> ValType {
        match self {
            K::I32(_) => ValType::I32,
            K::I64(_) => ValType::I64,
            K::F32(_) => ValType::F32,
            K::F64(_) => ValType::F64,
            K::V128(_) => ValType::V128,
        }
    }
    fn value(&self) -> Value {
        match *self {
            K::I32(v) => Value::I32(v),
            K::I64(v) => Value::I64(v),
            K::F32(v) => Value::F32(f32::from_bits(v)),
            K::F64(v) => Value::F64(f64::from_bits(v)),
            K::V128(v) => Value::V128(v),
        }
    }
    fn bytes(&self) -> usize {
        match *self {
            K::I32(v) => 1 + leb_len(v.unsigned_abs() as u64 * 2),
            K::I64(v) => 1 + leb_len(v.unsigned_abs() * 2),
            K::F32(_) => 5,
            K::F64(_) => 9,
            K::V128(_) => 18,
        }
    }
}

/// The body with every constant immediate cut out, and the constants in order.
fn shape(body: &[u8]) -> Option<(Vec<u8>, Vec<K>)> {
    let fb = FunctionBody::new(BinaryReader::new(body, 0));
    let mut ops = fb.get_operators_reader().ok()?;
    let start = ops.original_position();
    let mut out = body[..start].to_vec();
    let mut ks = Vec::new();
    while !ops.eof() {
        let (op, at) = ops.read_with_offset().ok()?;
        let end = ops.original_position();
        let k = match op {
            Operator::I32Const { value } => Some(K::I32(value)),
            Operator::I64Const { value } => Some(K::I64(value)),
            Operator::F32Const { value } => Some(K::F32(value.bits())),
            Operator::F64Const { value } => Some(K::F64(value.bits())),
            Operator::V128Const { value } => Some(K::V128(u128::from_le_bytes(*value.bytes()))),
            _ => None,
        };
        match k {
            Some(k) => {
                out.push(body[at]); // the opcode (0xfd-prefixed v128.const keeps its first byte; length differs)
                if matches!(k, K::V128(_)) {
                    out.push(0xfd);
                }
                ks.push(k);
            }
            None => out.extend_from_slice(&body[at..end]),
        }
    }
    Some((out, ks))
}

/// Visit the snapshot's instructions in emission order (the order of the binary).
fn in_order<'a>(snap: &'a mut Snapshot, seq: InstrSeqId, f: &mut dyn FnMut(&mut Instr)) {
    let n = snap.seqs[&seq].1.len();
    for k in 0..n {
        let kids = children(&snap.seqs[&seq].1[k]);
        f(&mut snap.seqs.get_mut(&seq).unwrap().1[k]);
        for c in kids {
            in_order(snap, c, f);
        }
    }
}

/// Members of one (type index, shape) group: (function, constants, body length).
type Group = Vec<(FunctionId, Vec<K>, usize)>;

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();
    let enc = encode_functions(m);
    let mut groups: HashMap<(u32, Vec<u8>), Group> = HashMap::new();
    let mut ids: Vec<_> = enc.bodies.keys().copied().collect();
    ids.sort();
    let shapes: Vec<_> = ids
        .into_par_iter()
        .map(|id| {
            let (ty, body) = &enc.bodies[&id];
            let sh = if body.len() < 16 { None } else { shape(body) };
            (id, *ty, body.len(), sh)
        })
        .collect();
    for (id, ty, len, sh) in shapes {
        if let Some((sh, ks)) = sh
            && !ks.is_empty()
        {
            groups.entry((ty, sh)).or_default().push((id, ks, len));
        }
    }
    let mut groups: Vec<_> = groups.into_iter().filter(|(_, v)| v.len() > 1).collect();
    groups.sort_by_key(|(_, v)| v[0].0);
    for ((_, _), members) in groups {
        let rep = members[0].0;
        let nk = members[0].1.len();
        let differs: Vec<usize> = (0..nk)
            .filter(|&i| members.iter().any(|(_, ks, _)| ks[i] != members[0].1[i]))
            .collect();
        if differs.is_empty() || differs.len() > 32 {
            continue;
        }
        let f = m.funcs.get(rep).kind.unwrap_local();
        if !inlinable(f) {
            continue;
        }
        let ty = m.types.get(f.ty());
        let (params, results) = (ty.params().to_vec(), ty.results().to_vec());
        // Profitability: k bodies become one body plus k thunks.
        let k = members.len() as i64;
        let body = members[0].2 as i64;
        let thunk = |ks: &Vec<K>| -> i64 {
            2 * params.len() as i64
                + differs.iter().map(|&i| ks[i].bytes() as i64).sum::<i64>()
                + 3
                + 3
        };
        let new_body = body + differs.len() as i64 * 2;
        let new_type = 3 + params.len() as i64 + differs.len() as i64 + results.len() as i64;
        let after =
            new_body + new_type + 4 + members.iter().map(|(_, ks, _)| thunk(ks) + 3).sum::<i64>();
        let before = k * (body + 3);
        if after >= before {
            continue;
        }
        // Build the shared function from the representative.
        let mut snap = Snapshot::take(m, f);
        let extra: Vec<LocalId> = differs
            .iter()
            .map(|&i| m.locals.add(members[0].1[i].ty()))
            .collect();
        let mut seen = 0usize;
        let mut next = 0usize;
        let entry = snap.entry;
        in_order(&mut snap, entry, &mut |i: &mut Instr| {
            if let Instr::Const(_) = i {
                if next < differs.len() && differs[next] == seen {
                    *i = LocalGet { local: extra[next] }.into();
                    next += 1;
                }
                seen += 1;
            }
        });
        if seen != nk {
            // The IR and the binary disagree about constants; leave this group.
            continue;
        }
        let mut all_params = params.clone();
        all_params.extend(differs.iter().map(|&i| members[0].1[i].ty()));
        let mut b = FunctionBuilder::new(&mut m.types, &all_params, &results);
        let body_id = b.func_body_id();
        let mut locals: HashMap<LocalId, LocalId> = HashMap::new();
        let mut args = Vec::new();
        for &a in &snap.args {
            let n = m.locals.add(m.locals.get(a).ty());
            locals.insert(a, n);
            args.push(n);
        }
        for &l in &snap.locals {
            if !extra.contains(&l) {
                locals.insert(l, m.locals.add(m.locals.get(l).ty()));
            }
        }
        for &e in &extra {
            locals.insert(e, e);
            args.push(e);
        }
        let mut seq_map = HashMap::new();
        seq_map.insert(entry, body_id);
        copy_seq(
            &snap,
            entry,
            body_id,
            &mut b,
            &mut seq_map,
            &locals,
            body_id,
        );
        let shared = m.funcs.add_local(b.local_func(args));

        // Each member becomes a thunk.
        for (id, ks, _) in &members {
            let f = m.funcs.get_mut(*id).kind.unwrap_local_mut();
            let args = f.args.clone();
            let entry = f.entry_block();
            let mut instrs: Vec<(Instr, InstrLocId)> = args
                .iter()
                .map(|&a| (LocalGet { local: a }.into(), InstrLocId::default()))
                .collect();
            for &i in &differs {
                instrs.push((
                    Const {
                        value: ks[i].value(),
                    }
                    .into(),
                    InstrLocId::default(),
                ));
            }
            instrs.push((Call { func: shared }.into(), InstrLocId::default()));
            f.block_mut(entry).instrs = instrs;
            stats.merged += 1;
        }
        stats.groups += 1;
        stats.params_added += differs.len();
    }
    stats
}
