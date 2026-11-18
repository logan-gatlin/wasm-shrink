//! Helpers shared by the passes: function-reference rewriting, use counting,
//! and the encoded bytes of each function body.

use rayon::prelude::*;
use std::collections::HashMap;
use walrus::ir::{Instr, Visitor, VisitorMut, dfs_in_order, dfs_pre_order_mut};
use walrus::{
    ConstExpr, ElementItems, ExportItem, FunctionId, FunctionKind, GlobalKind, LocalFunction,
    Module,
};

/// Local function ids, in the module's own order.
pub fn local_ids(m: &Module) -> Vec<FunctionId> {
    m.funcs.iter_local().map(|(id, _)| id).collect()
}

/// Run `f` on every local function in parallel. Results come back in
/// [`local_ids`] order, so a caller that then changes the module in that order
/// (allocating locals, say) gets the same result as a sequential loop.
pub fn par_map<T: Send>(
    m: &Module,
    f: impl Fn(FunctionId, &LocalFunction) -> T + Sync,
) -> Vec<(FunctionId, T)> {
    local_ids(m)
        .into_par_iter()
        .map(|id| (id, f(id, m.funcs.get(id).kind.unwrap_local())))
        .collect()
}

/// Edit every local function in parallel while `f` reads the rest of the
/// module. Each function is taken out and replaced by a placeholder with the
/// same signature, so calls to it still see its type. `f` must not rely on
/// any other function's body. Results come back in [`local_ids`] order.
pub fn par_take<T: Send>(
    m: &mut Module,
    f: impl Fn(&Module, FunctionId, &mut LocalFunction) -> T + Sync,
) -> Vec<(FunctionId, T)> {
    let mut taken: Vec<(FunctionId, LocalFunction)> = local_ids(m)
        .into_iter()
        .map(|id| {
            let ph = placeholder(m, id);
            (
                id,
                std::mem::replace(m.funcs.get_mut(id).kind.unwrap_local_mut(), ph),
            )
        })
        .collect();
    let out = {
        let m: &Module = m;
        taken
            .par_iter_mut()
            .map(|(id, lf)| (*id, f(m, *id, lf)))
            .collect()
    };
    for (id, lf) in taken {
        *m.funcs.get_mut(id).kind.unwrap_local_mut() = lf;
    }
    out
}

/// An empty function with the same signature as `id`.
pub fn placeholder(m: &mut Module, id: FunctionId) -> LocalFunction {
    let ty = m.types.get(m.funcs.get(id).ty());
    let (p, r) = (ty.params().to_vec(), ty.results().to_vec());
    walrus::FunctionBuilder::new(&mut m.types, &p, &r).local_func(vec![])
}

/// `impl AddAssign` for a stats struct whose fields are all counters, so
/// per-function stats from a parallel pass can be summed.
#[macro_export]
macro_rules! sum_stats {
    ($t:ty: $($f:ident),* $(,)?) => {
        impl std::ops::AddAssign for $t {
            fn add_assign(&mut self, o: Self) {
                $(self.$f += o.$f;)*
            }
        }
    };
}

/// Point every reference to a key of `map` at its value: calls, `ref.func`,
/// tables, exports, globals and the start function.
pub fn replace_functions(m: &mut Module, map: &HashMap<FunctionId, FunctionId>) {
    if map.is_empty() {
        return;
    }
    let resolve = |mut f: FunctionId| {
        while let Some(&g) = map.get(&f) {
            f = g;
        }
        f
    };
    struct Rewrite<'a, F: Fn(FunctionId) -> FunctionId>(&'a F);
    impl<F: Fn(FunctionId) -> FunctionId> VisitorMut for Rewrite<'_, F> {
        fn visit_function_id_mut(&mut self, f: &mut FunctionId) {
            *f = (self.0)(*f);
        }
    }
    m.funcs.par_iter_local_mut().for_each(|(_, f)| {
        let entry = f.entry_block();
        dfs_pre_order_mut(&mut Rewrite(&resolve), f, entry);
    });
    let fix_expr = |e: &mut ConstExpr| {
        if let ConstExpr::RefFunc(f) = e {
            *f = resolve(*f);
        }
    };
    for el in m.elements.iter_mut() {
        match &mut el.items {
            ElementItems::Functions(fs) => fs.iter_mut().for_each(|f| *f = resolve(*f)),
            ElementItems::Expressions(_, es) => es.iter_mut().for_each(fix_expr),
        }
    }
    for ex in m.exports.iter_mut() {
        if let ExportItem::Function(f) = &mut ex.item {
            *f = resolve(*f);
        }
    }
    let globals: Vec<_> = m.globals.iter().map(|g| g.id()).collect();
    for g in globals {
        if let GlobalKind::Local(e) = &mut m.globals.get_mut(g).kind {
            fix_expr(e);
        }
    }
    if let Some(s) = m.start {
        m.start = Some(resolve(s));
    }
}

/// How each function is used.
#[derive(Default, Clone, Debug)]
pub struct Uses {
    /// Direct `call` sites, as (caller, count).
    pub calls: HashMap<FunctionId, usize>,
    /// Any use that is not a direct `call`: `ref.func`, `return_call`, tables,
    /// exports, globals, the start function.
    pub pinned: bool,
}

impl Uses {
    pub fn call_count(&self) -> usize {
        self.calls.values().sum()
    }
}

pub fn function_uses(m: &Module) -> HashMap<FunctionId, Uses> {
    // Per function, in parallel: every function it refers to, and whether the
    // reference is a direct `call`.
    struct Collect(Vec<(FunctionId, bool)>);
    impl<'i> Visitor<'i> for Collect {
        fn visit_instr(&mut self, instr: &'i Instr, _: &'i walrus::InstrLocId) {
            match instr {
                Instr::Call(c) => self.0.push((c.func, true)),
                Instr::ReturnCall(c) => self.0.push((c.func, false)),
                Instr::RefFunc(r) => self.0.push((r.func, false)),
                _ => {}
            }
        }
    }
    let refs = par_map(m, |_, f| {
        let mut c = Collect(Vec::new());
        dfs_in_order(&mut c, f, f.entry_block());
        c.0
    });
    let mut uses: HashMap<FunctionId, Uses> = HashMap::new();
    for (caller, refs) in refs {
        for (g, call) in refs {
            let u = uses.entry(g).or_default();
            if call {
                *u.calls.entry(caller).or_default() += 1;
            } else {
                u.pinned = true;
            }
        }
    }
    let mut pin = |f: FunctionId| uses.entry(f).or_default().pinned = true;
    for el in m.elements.iter() {
        match &el.items {
            ElementItems::Functions(fs) => fs.iter().for_each(|f| pin(*f)),
            ElementItems::Expressions(_, es) => es.iter().for_each(|e| {
                if let ConstExpr::RefFunc(f) = e {
                    pin(*f)
                }
            }),
        }
    }
    for ex in m.exports.iter() {
        if let ExportItem::Function(f) = ex.item {
            pin(f);
        }
    }
    for g in m.globals.iter() {
        if let GlobalKind::Local(ConstExpr::RefFunc(f)) = g.kind {
            pin(f);
        }
    }
    if let Some(s) = m.start {
        pin(s);
    }
    uses
}

/// The encoded form of every local function, as walrus would emit it now.
pub struct Encoded {
    /// Type index and body bytes (locals + instructions + `end`).
    pub bodies: HashMap<FunctionId, (u32, Vec<u8>)>,
    /// Function index in the emitted module.
    pub index: HashMap<FunctionId, u32>,
}

/// Emit the module once and read each function body back.
///
/// Functions are matched to their emitted index by temporarily giving each a
/// unique name and reading the name section; the real names are restored.
pub fn encode_functions(m: &mut Module) -> Encoded {
    let ids: Vec<FunctionId> = m.funcs.iter().map(|f| f.id()).collect();
    let saved: Vec<Option<String>> = ids.iter().map(|&id| m.funcs.get(id).name.clone()).collect();
    for &id in &ids {
        m.funcs.get_mut(id).name = Some(format!("\u{1}{}", id.index()));
    }
    // The module must be parsed with the default config, which emits names.
    let wasm = m.emit_wasm();
    for (&id, name) in ids.iter().zip(saved) {
        m.funcs.get_mut(id).name = name;
    }
    let by_tag: HashMap<String, FunctionId> = ids
        .iter()
        .map(|id| (format!("\u{1}{}", id.index()), *id))
        .collect();

    let mut types = Vec::new();
    let mut bodies = Vec::new();
    let mut imported = 0u32;
    let mut names: HashMap<u32, FunctionId> = HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
        match payload.expect("walrus emitted an invalid module") {
            wasmparser::Payload::ImportSection(s) => {
                for imp in s.into_imports() {
                    if matches!(imp.unwrap().ty, wasmparser::TypeRef::Func(_)) {
                        imported += 1;
                    }
                }
            }
            wasmparser::Payload::FunctionSection(s) => {
                types.extend(s.into_iter().map(|t| t.unwrap()))
            }
            wasmparser::Payload::CodeSectionEntry(b) => bodies.push(wasm[b.range()].to_vec()),
            wasmparser::Payload::CustomSection(c) => {
                if let wasmparser::KnownCustom::Name(r) = c.as_known() {
                    for sub in r {
                        if let Ok(wasmparser::Name::Function(map)) = sub {
                            for n in map {
                                let n = n.unwrap();
                                if let Some(&id) = by_tag.get(n.name) {
                                    names.insert(n.index, id);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Encoded {
        bodies: HashMap::new(),
        index: HashMap::new(),
    };
    for (&idx, &id) in &names {
        out.index.insert(id, idx);
        if idx >= imported {
            let k = (idx - imported) as usize;
            out.bodies
                .insert(id, (types[k], std::mem::take(&mut bodies[k])));
        }
    }
    // Restore the module's own view of which functions are local.
    debug_assert!(
        out.bodies
            .keys()
            .all(|id| matches!(m.funcs.get(*id).kind, FunctionKind::Local(_)))
    );
    out
}

/// Unsigned LEB128 length of `v`.
pub fn leb_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}
