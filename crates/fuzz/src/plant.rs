use arbitrary::{Result, Unstructured};
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{
    ConstExpr, DataKind, ElementItems, FunctionBuilder, FunctionId, FunctionKind, GlobalId,
    LocalFunction, LocalId, MemoryId, Module, ValType,
};

pub fn run(wasm: &[u8], u: &mut Unstructured) -> Result<Vec<u8>> {
    let Ok(mut m) = Module::from_buffer(wasm) else {
        return Ok(wasm.to_vec());
    };
    shapes(&mut m, u)?;
    copies(&m.emit_wasm(), u)
}

fn shapes(m: &mut Module, u: &mut Unstructured) -> Result<()> {
    let mut mem = m
        .memories
        .iter()
        .find(|m| !m.memory64 && m.import.is_none())
        .map(|m| m.id());
    if mem.is_none() && m.memories.iter().all(|m| m.import.is_none()) && u.arbitrary()? {
        let id = m.memories.add_local(false, false, 1, Some(1), None);
        m.exports.add("planted_memory", id);
        mem = Some(id);
    }
    let local: Vec<FunctionId> = m.funcs.iter_local().map(|(id, _)| id).collect();
    if let Some(mem) = mem {
        for _ in 0..u.int_in_range(0..=3)? {
            let f = *u.choose(&local)?;
            frame_addresses(m, f, mem, u)?;
        }
        let memory = m.memories.get(mem);
        let bytes = memory.initial << memory.page_size_log2.unwrap_or(16);
        if bytes >= 64 && u.arbitrary()? {
            let len = u.int_in_range(1..=bytes.min(512))?;
            let offset = u.int_in_range(0..=bytes - len)?;
            let mut value: Vec<u8> = (0..len).map(|_| u.arbitrary()).collect::<Result<_>>()?;
            zero_runs(&mut value, u)?;
            m.data.add(
                DataKind::Active {
                    memory: mem,
                    offset: ConstExpr::Value(Value::I32(offset as i32)),
                },
                value,
            );
        }
    }

    let data: Vec<_> = m.data.iter().map(|d| d.id()).collect();
    for id in data {
        let d = m.data.get_mut(id);
        if matches!(d.kind, DataKind::Active { .. }) && !d.value.is_empty() && u.arbitrary()? {
            zero_runs(&mut d.value, u)?;
        }
    }

    let observer = m
        .globals
        .add_local(ValType::I32, true, false, ConstExpr::Value(Value::I32(0)));
    m.exports.add("planted_observer", observer);
    for _ in 0..u.int_in_range(0..=3)? {
        let f = *u.choose(&local)?;
        constant_sets(m, f, observer, u)?;
    }
    for _ in 0..u.int_in_range(0..=3)? {
        let f = *u.choose(&local)?;
        tails(m, f, observer, u)?;
    }
    for _ in 0..u.int_in_range(0..=3)? {
        helper(m, &local, observer, u)?;
    }

    Ok(())
}

fn copies(wasm: &[u8], u: &mut Unstructured) -> Result<Vec<u8>> {
    let locals = local_count(wasm);
    if locals == 0 {
        return Ok(wasm.to_vec());
    }
    let mut exact = Vec::new();
    let mut similar = Vec::new();
    for _ in 0..u.int_in_range(0..=6)? {
        let k = u.choose_index(locals)?;
        if u.ratio(1, 2)? {
            exact.push(k);
        } else {
            similar.push(k);
        }
    }
    let picks: Vec<usize> = exact.iter().chain(&similar).copied().collect();
    let wasm = append_clones(wasm, &picks).unwrap_or_else(|| wasm.to_vec());
    let Ok(mut m) = Module::from_buffer(&wasm) else {
        return Ok(wasm);
    };
    let ids: Vec<FunctionId> = m.funcs.iter_local().map(|(id, _)| id).collect();
    if ids.len() != locals + picks.len() {
        return Ok(wasm);
    }

    let mut alias: HashMap<FunctionId, Vec<FunctionId>> = HashMap::new();
    let mut n = 0;
    for (j, &k) in exact.iter().enumerate() {
        let clone = ids[locals + j];
        alias.entry(ids[k]).or_default().push(clone);
        if u.ratio(1, 3)? {
            export(&mut m, clone, &mut n);
        }
    }
    for (j, _) in similar.iter().enumerate() {
        let clone = ids[locals + exact.len() + j];
        perturb_constants(&mut m, clone, u)?;
        export(&mut m, clone, &mut n);
    }

    let all: Vec<FunctionId> = m.funcs.iter().map(|f| f.id()).collect();
    for _ in 0..u.int_in_range(0..=4)? {
        let g = *u.choose(&all)?;
        let swap = u.ratio(1, 4)?;
        if let Some(t) = thunk(&mut m, g, swap) {
            if swap {
                export(&mut m, t, &mut n);
            } else {
                alias.entry(g).or_default().push(t);
                if u.ratio(1, 3)? {
                    export(&mut m, t, &mut n);
                }
            }
        }
    }
    if !alias.is_empty() {
        redirect(&mut m, &alias, u)?;
    }

    Ok(m.emit_wasm())
}

fn zero_runs(value: &mut [u8], u: &mut Unstructured) -> Result<()> {
    for _ in 0..u.int_in_range(1..=4)? {
        let at = u.choose_index(value.len())?;
        let run = u.int_in_range(1..=value.len() - at)?;
        value[at..at + run].fill(0);
    }
    Ok(())
}

fn observe(g: GlobalId, value: Vec<Instr>) -> Vec<Instr> {
    let mut code = vec![GlobalGet { global: g }.into()];
    code.extend(value);
    code.extend([
        Binop {
            op: BinaryOp::I32Xor,
        }
        .into(),
        Const {
            value: Value::I32(0x9e37_79b9u32 as i32),
        }
        .into(),
        Binop {
            op: BinaryOp::I32Mul,
        }
        .into(),
        GlobalSet { global: g }.into(),
    ]);
    code
}

fn constant_sets(m: &mut Module, f: FunctionId, g: GlobalId, u: &mut Unstructured) -> Result<()> {
    let x = m.locals.add(ValType::I32);
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let pool: Vec<i32> = (0..u.int_in_range(1..=2)?)
        .map(|_| u.int_in_range(-2..=300))
        .collect::<Result<_>>()?;
    let all = seqs(lf);
    for _ in 0..u.int_in_range(2..=8)? {
        let (s, at) = position(lf, &all, u)?;
        let code = if u.ratio(2, 3)? {
            vec![
                Const {
                    value: Value::I32(*u.choose(&pool)?),
                }
                .into(),
                LocalSet { local: x }.into(),
            ]
        } else {
            observe(g, vec![LocalGet { local: x }.into()])
        };
        insert(lf, s, at, code);
    }
    Ok(())
}

fn tails(m: &mut Module, f: FunctionId, g: GlobalId, u: &mut Unstructured) -> Result<()> {
    let no_results = m.types.get(m.funcs.get(f).ty()).results().is_empty();
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let conds: Vec<LocalId> = lf
        .args
        .iter()
        .copied()
        .filter(|&a| m.locals.get(a).ty() == ValType::I32)
        .collect();
    let mut tail = observe(
        g,
        vec![
            Const {
                value: Value::I32(u.arbitrary()?),
            }
            .into(),
        ],
    );
    tail.push(if no_results && u.arbitrary()? {
        Return {}.into()
    } else {
        Unreachable {}.into()
    });
    let all = seqs(lf);
    for _ in 0..u.int_in_range(2..=4)? {
        let (s, at) = position(lf, &all, u)?;
        let then = lf
            .builder_mut()
            .dangling_instr_seq(InstrSeqType::Simple(None))
            .id();
        let alt = lf
            .builder_mut()
            .dangling_instr_seq(InstrSeqType::Simple(None))
            .id();
        insert(lf, then, 0, tail.clone());
        let mut code: Vec<Instr> = match u.choose(&conds) {
            Ok(&c) => vec![
                LocalGet { local: c }.into(),
                Const {
                    value: Value::I32(u.int_in_range(-4..=64)?),
                }
                .into(),
                Binop {
                    op: BinaryOp::I32LtS,
                }
                .into(),
            ],
            Err(_) => vec![GlobalGet { global: g }.into()],
        };
        code.push(
            IfElse {
                consequent: then,
                alternative: alt,
            }
            .into(),
        );
        insert(lf, s, at, code);
    }
    Ok(())
}

fn helper(m: &mut Module, callers: &[FunctionId], g: GlobalId, u: &mut Unstructured) -> Result<()> {
    use walrus::ir::BinaryOp::*;
    let p = m.locals.add(ValType::I32);
    let acc = m.locals.add(ValType::I32);
    let c = m.locals.add(ValType::I32);
    let mut b = FunctionBuilder::new(&mut m.types, &[ValType::I32], &[ValType::I32]);
    let mut body = b.func_body();
    for _ in 0..u.int_in_range(1..=4)? {
        match u.int_in_range(0..=2)? {
            0 => {
                let x = if u.arbitrary()? { acc } else { c };
                body.local_get(x).local_get(p).binop(I32Add).local_set(x);
            }
            1 => {
                let n = u.int_in_range(1..=4)?;
                body.loop_(None, |l| {
                    let id = l.id();
                    l.local_get(c)
                        .i32_const(1)
                        .binop(I32Add)
                        .local_tee(c)
                        .i32_const(n)
                        .binop(I32LtU)
                        .br_if(id);
                });
            }
            _ => {
                let k = u.int_in_range(-4..=40)?;
                body.local_get(p).i32_const(k).binop(I32LtS).if_else(
                    None,
                    |t| {
                        t.local_get(acc).return_();
                    },
                    |_| {},
                );
            }
        }
    }
    match u.int_in_range(0..=2)? {
        0 => body.local_get(acc),
        1 => body.local_get(c),
        _ => body.local_get(acc).local_get(c).binop(I32Xor),
    };
    let h = b.finish(vec![p], &mut m.funcs);
    for _ in 0..u.int_in_range(1..=2)? {
        let f = *u.choose(callers)?;
        let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
            continue;
        };
        let all = seqs(lf);
        let (s, at) = position(lf, &all, u)?;
        let arg = u.int_in_range(-8..=48)?;
        let code = observe(
            g,
            vec![
                Const {
                    value: Value::I32(arg),
                }
                .into(),
                Call { func: h }.into(),
            ],
        );
        if u.arbitrary()? {
            let n = u.int_in_range(2..=4)?;
            let k = m.locals.add(ValType::I32);
            let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
                continue;
            };
            let body = lf
                .builder_mut()
                .dangling_instr_seq(InstrSeqType::Simple(None))
                .id();
            insert(lf, body, 0, code);
            let len = lf.block(body).instrs.len();
            insert(
                lf,
                body,
                len,
                [
                    LocalGet { local: k }.into(),
                    Const {
                        value: Value::I32(1),
                    }
                    .into(),
                    Binop {
                        op: BinaryOp::I32Sub,
                    }
                    .into(),
                    LocalTee { local: k }.into(),
                    BrIf { block: body }.into(),
                ],
            );
            insert(
                lf,
                s,
                at,
                [
                    Const {
                        value: Value::I32(n),
                    }
                    .into(),
                    LocalSet { local: k }.into(),
                    Loop { seq: body }.into(),
                ],
            );
        } else {
            insert(lf, s, at, code);
        }
    }
    Ok(())
}

fn local_count(wasm: &[u8]) -> usize {
    for p in wasmparser::Parser::new(0).parse_all(wasm) {
        match p {
            Ok(wasmparser::Payload::FunctionSection(r)) => return r.count() as usize,
            Err(_) => return 0,
            _ => {}
        }
    }
    0
}

fn append_clones(wasm: &[u8], picks: &[usize]) -> Option<Vec<u8>> {
    use wasm_encoder::{CodeSection, FunctionSection, RawSection};
    use wasmparser::Payload;
    if picks.is_empty() {
        return None;
    }
    let mut out = wasm_encoder::Module::new();
    let mut types: Vec<u32> = Vec::new();
    let mut bodies: Vec<&[u8]> = Vec::new();
    for p in wasmparser::Parser::new(0).parse_all(wasm) {
        match p.ok()? {
            Payload::Version { .. } | Payload::End(_) | Payload::CodeSectionStart { .. } => {}
            Payload::FunctionSection(r) => {
                types = r.into_iter().collect::<Result<_, _>>().ok()?;
                let mut s = FunctionSection::new();
                for &t in types.iter().chain(picks.iter().map(|&k| &types[k])) {
                    s.function(t);
                }
                out.section(&s);
            }
            Payload::CodeSectionEntry(body) => {
                bodies.push(&wasm[body.range()]);
                if bodies.len() == types.len() {
                    let mut s = CodeSection::new();
                    for b in bodies.iter().chain(picks.iter().map(|&k| &bodies[k])) {
                        s.raw(b);
                    }
                    out.section(&s);
                }
            }
            p => {
                let (id, range) = p.as_section()?;
                out.section(&RawSection {
                    id,
                    data: &wasm[range],
                });
            }
        }
    }
    let out = out.finish();
    crate::validate(&out).ok()?;
    Some(out)
}

fn export(m: &mut Module, f: FunctionId, n: &mut usize) {
    m.exports.add(&format!("planted{n}"), f);
    *n += 1;
}

fn perturb_constants(m: &mut Module, f: FunctionId, u: &mut Unstructured) -> Result<()> {
    struct V<'a, 'b> {
        u: &'a mut Unstructured<'b>,
        err: Option<arbitrary::Error>,
    }
    impl VisitorMut for V<'_, '_> {
        fn visit_value_mut(&mut self, v: &mut Value) {
            let r = (|| -> Result<()> {
                if self.u.ratio(2, 3)? {
                    return Ok(());
                }
                match v {
                    Value::I32(x) => *x = self.u.arbitrary()?,
                    Value::I64(x) => *x = self.u.arbitrary()?,
                    Value::F32(x) => *x = f32::from_bits(self.u.arbitrary()?),
                    Value::F64(x) => *x = f64::from_bits(self.u.arbitrary()?),
                    Value::V128(x) => *x = self.u.arbitrary()?,
                }
                Ok(())
            })();
            if let Err(e) = r {
                self.err.get_or_insert(e);
            }
        }
    }
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let entry = lf.entry_block();
    let mut v = V { u, err: None };
    dfs_pre_order_mut(&mut v, lf, entry);
    v.err.map_or(Ok(()), Err)
}

fn thunk(m: &mut Module, g: FunctionId, swap: bool) -> Option<FunctionId> {
    let ty = m.funcs.get(g).ty();
    let (params, results) = {
        let t = m.types.get(ty);
        (t.params().to_vec(), t.results().to_vec())
    };
    if m.types.add(&params, &results) != ty {
        return None;
    }
    let mut order: Vec<usize> = (0..params.len()).collect();
    if swap {
        let (a, b) = (0..params.len())
            .flat_map(|a| (a + 1..params.len()).map(move |b| (a, b)))
            .find(|&(a, b)| params[a] == params[b])?;
        order.swap(a, b);
    }
    let args: Vec<LocalId> = params.iter().map(|&t| m.locals.add(t)).collect();
    let mut b = FunctionBuilder::new(&mut m.types, &params, &results);
    let mut body = b.func_body();
    for &i in &order {
        body.local_get(args[i]);
    }
    body.call(g);
    Some(b.finish(args, &mut m.funcs))
}

fn redirect(
    m: &mut Module,
    alias: &HashMap<FunctionId, Vec<FunctionId>>,
    u: &mut Unstructured,
) -> Result<()> {
    struct V<'a, 'b> {
        alias: &'a HashMap<FunctionId, Vec<FunctionId>>,
        u: &'a mut Unstructured<'b>,
        err: Option<arbitrary::Error>,
    }
    impl V<'_, '_> {
        fn pick(&mut self, f: &mut FunctionId) {
            let Some(to) = self.alias.get(f) else { return };
            let r = (|| -> Result<()> {
                if self.u.arbitrary()? {
                    *f = *self.u.choose(to)?;
                }
                Ok(())
            })();
            if let Err(e) = r {
                self.err.get_or_insert(e);
            }
        }
    }
    impl VisitorMut for V<'_, '_> {
        fn visit_function_id_mut(&mut self, f: &mut FunctionId) {
            self.pick(f);
        }
    }
    let mut v = V {
        alias,
        u,
        err: None,
    };
    for (_, lf) in m.funcs.iter_local_mut() {
        let entry = lf.entry_block();
        dfs_pre_order_mut(&mut v, lf, entry);
    }
    for e in m.elements.iter_mut() {
        match &mut e.items {
            ElementItems::Functions(fs) => fs.iter_mut().for_each(|f| v.pick(f)),
            ElementItems::Expressions(_, es) => {
                for e in es {
                    if let ConstExpr::RefFunc(f) = e {
                        v.pick(f);
                    }
                }
            }
        }
    }
    v.err.map_or(Ok(()), Err)
}

fn seqs(f: &LocalFunction) -> Vec<InstrSeqId> {
    struct V(Vec<InstrSeqId>);
    impl<'i> Visitor<'i> for V {
        fn start_instr_seq(&mut self, s: &'i InstrSeq) {
            self.0.push(s.id());
        }
    }
    let mut v = V(Vec::new());
    dfs_in_order(&mut v, f, f.entry_block());
    v.0
}

fn frame_addresses(
    m: &mut Module,
    f: FunctionId,
    memory: MemoryId,
    u: &mut Unstructured,
) -> Result<()> {
    let new_local = m.locals.add(ValType::I32);
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let i32_param = lf
        .args
        .iter()
        .copied()
        .filter(|&a| m.locals.get(a).ty() == ValType::I32)
        .collect::<Vec<_>>();
    let entry = lf.entry_block();
    let fp = if !i32_param.is_empty() && u.arbitrary()? {
        *u.choose(&i32_param)?
    } else {
        let at = u.int_in_range(0..=lf.block(entry).instrs.len())?;
        let base = u.int_in_range(0..=1024)? & !3;
        let def = [
            Const {
                value: Value::I32(base),
            }
            .into(),
            LocalSet { local: new_local }.into(),
        ];
        insert(lf, entry, at, def);
        new_local
    };
    let pool: Vec<i32> = (0..u.int_in_range(1..=3)?)
        .map(|_| Ok(u.int_in_range(-16..=256)? & !3))
        .collect::<Result<_>>()?;
    let all = seqs(lf);
    for _ in 0..u.int_in_range(2..=10)? {
        let (s, at) = position(lf, &all, u)?;
        let addr = |c: i32| -> [Instr; 3] {
            [
                LocalGet { local: fp }.into(),
                Const {
                    value: Value::I32(c),
                }
                .into(),
                Binop {
                    op: BinaryOp::I32Add,
                }
                .into(),
            ]
        };
        let arg = MemArg {
            align: 4,
            offset: u.int_in_range(0..=2)? * 4,
        };
        let load = Load {
            memory,
            kind: LoadKind::I32 { atomic: false },
            arg,
        };
        let store = Store {
            memory,
            kind: StoreKind::I32 { atomic: false },
            arg,
        };
        let mut code: Vec<Instr> = Vec::new();
        match u.int_in_range(0..=7)? {
            0 => code.extend([
                Const {
                    value: Value::I32(u.arbitrary()?),
                }
                .into(),
                LocalSet { local: fp }.into(),
            ]),
            1..=3 => {
                code.extend(addr(*u.choose(&pool)?));
                code.extend(addr(*u.choose(&pool)?));
                code.extend([load.into(), store.into()]);
            }
            _ => {
                let c = *u.choose(&pool)?;
                code.extend(addr(c));
                code.extend(addr(c));
                code.extend([
                    load.into(),
                    Const {
                        value: Value::I32(1),
                    }
                    .into(),
                    Binop {
                        op: BinaryOp::I32Add,
                    }
                    .into(),
                    store.into(),
                ]);
            }
        }
        insert(lf, s, at, code);
    }
    Ok(())
}

fn position(
    f: &LocalFunction,
    all: &[InstrSeqId],
    u: &mut Unstructured,
) -> Result<(InstrSeqId, usize)> {
    let s = if u.arbitrary()? {
        f.entry_block()
    } else {
        *u.choose(all)?
    };
    let len = f.block(s).instrs.len();
    let at = if u.arbitrary()? {
        u.int_in_range(0..=len.min(2))?
    } else {
        u.int_in_range(0..=len)?
    };
    Ok((s, at))
}

fn insert(f: &mut LocalFunction, s: InstrSeqId, at: usize, code: impl IntoIterator<Item = Instr>) {
    let code: Vec<_> = code
        .into_iter()
        .map(|i| (i, InstrLocId::default()))
        .collect();
    f.block_mut(s).instrs.splice(at..at, code);
}
