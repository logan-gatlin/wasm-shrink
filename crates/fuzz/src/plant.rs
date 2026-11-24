use arbitrary::{Result, Unstructured};
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{
    AbstractHeapType, ConstExpr, DataKind, ElementItems, ElementKind, FunctionBuilder, FunctionId,
    FunctionKind, GlobalId, HeapType, LocalFunction, LocalId, MemoryId, Module, RawCustomSection,
    RefType, ValType,
};

/// Integers on either side of where a signed LEB128 gets a byte longer, and
/// the ends of the 32- and 64-bit ranges.
const EDGES: &[i64] = &[
    0,
    -1,
    63,
    64,
    -64,
    -65,
    8191,
    8192,
    -8192,
    -8193,
    i32::MAX as i64,
    i32::MIN as i64,
    u32::MAX as i64,
    i64::MAX,
    i64::MIN,
];

/// Floats that compare equal, or do not compare at all, but have different
/// bits: both zeros and NaNs with different payloads.
const FLOATS: &[u64] = &[
    0,
    1 << 63,
    0x7ff8_0000_0000_0000,
    0x7ff8_0000_0000_0001,
    0xfff8_0000_0000_0000,
    0x7ff0_0000_0000_0001,
];

fn f32_bits(bits: u64) -> u32 {
    // Keep the sign, the exponent and the low payload bit.
    ((bits >> 32) as u32 & 0xfff8_0000) | (bits as u32 & 1)
}

/// A constant of type `ty`, often one of [`EDGES`] or [`FLOATS`].
fn constant(ty: ValType, u: &mut Unstructured) -> Result<Value> {
    let edge = u.ratio(1, 2)?;
    Ok(match ty {
        ValType::I32 if edge => Value::I32(*u.choose(EDGES)? as i32),
        ValType::I32 => Value::I32(u.int_in_range(-2..=300)?),
        ValType::I64 if edge => Value::I64(*u.choose(EDGES)?),
        ValType::I64 => Value::I64(u.arbitrary()?),
        ValType::F32 if edge => Value::F32(f32::from_bits(f32_bits(*u.choose(FLOATS)?))),
        ValType::F32 => Value::F32(f32::from_bits(u.arbitrary()?)),
        ValType::F64 if edge => Value::F64(f64::from_bits(*u.choose(FLOATS)?)),
        _ => Value::F64(f64::from_bits(u.arbitrary()?)),
    })
}

/// Turn the value of type `ty` on the stack into an `i32` that depends on
/// all of its bits.
fn to_i32(ty: ValType) -> Vec<Instr> {
    let unop = |op| Instr::from(Unop { op });
    let fold = [
        Const {
            value: Value::I64(0x9e37_79b9_7f4a_7c15u64 as i64),
        }
        .into(),
        Binop {
            op: BinaryOp::I64Mul,
        }
        .into(),
        Const {
            value: Value::I64(32),
        }
        .into(),
        Binop {
            op: BinaryOp::I64ShrU,
        }
        .into(),
        unop(UnaryOp::I32WrapI64),
    ];
    match ty {
        ValType::I32 => vec![],
        ValType::F32 => vec![unop(UnaryOp::I32ReinterpretF32)],
        ValType::I64 => fold.to_vec(),
        _ => [unop(UnaryOp::I64ReinterpretF64)]
            .into_iter()
            .chain(fold)
            .collect(),
    }
}

const NUMBERS: [ValType; 4] = [ValType::I32, ValType::I64, ValType::F32, ValType::F64];

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
            // Mostly segments that fit. Now and then one touches or overlaps
            // the one before it, ends where memory ends, or runs past it.
            let mut last_end = 0u64;
            for _ in 0..u.int_in_range(1..=3)? {
                let len = u.int_in_range(0..=bytes.min(512))?;
                let fits = bytes - len;
                let offset = match u.int_in_range(0..=11)? {
                    0 => fits + u.int_in_range(1..=8)?,
                    1 => fits,
                    2 | 3 => last_end.saturating_sub(u.int_in_range(0..=4)?).min(fits),
                    _ => u.int_in_range(0..=fits)?,
                };
                let mut value: Vec<u8> = (0..len).map(|_| u.arbitrary()).collect::<Result<_>>()?;
                if u.ratio(1, 8)? {
                    value.fill(0);
                } else if !value.is_empty() {
                    zero_runs(&mut value, u)?;
                }
                let id = m.data.add(
                    DataKind::Active {
                        memory: mem,
                        offset: ConstExpr::Value(Value::I32(offset as i32)),
                    },
                    value,
                );
                m.memories.get_mut(mem).data_segments.insert(id);
                last_end = offset + len;
            }
        }
    }
    if u.ratio(1, 4)? {
        let data = (0..u.int_in_range(0..=16)?)
            .map(|_| u.arbitrary())
            .collect::<Result<_>>()?;
        m.customs.add(RawCustomSection {
            name: "planted".into(),
            data,
        });
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
        let f = *u.choose(&local)?;
        arms(m, f, observer, u)?;
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

    // A thunk can forward to an earlier thunk, so chains form.
    let mut all: Vec<FunctionId> = m.funcs.iter().map(|f| f.id()).collect();
    for _ in 0..u.int_in_range(0..=4)? {
        let g = *u.choose(&all)?;
        let swap = u.ratio(1, 4)?;
        if let Some(t) = thunk(&mut m, g, swap) {
            all.push(t);
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
    // Two thunks that forward to each other. A call to one runs out of
    // stack, so they are only reachable through the planted table.
    if u.ratio(1, 8)?
        && let Some(a) = thunk(&mut m, *u.choose(&all)?, false)
        && let Some(b) = thunk(&mut m, a, false)
    {
        let lf = m.funcs.get_mut(a).kind.unwrap_local_mut();
        let entry = lf.entry_block();
        if let Some((Instr::Call(call), _)) = lf.block_mut(entry).instrs.last_mut() {
            call.func = b;
        }
        all.push(b);
    }
    if !alias.is_empty() {
        redirect(&mut m, &alias, u)?;
    }
    if u.ratio(1, 2)? {
        table(&mut m, &all, u)?;
    }

    Ok(m.emit_wasm())
}

/// An exported table that starts out full of one function, with an element
/// segment written over it. The check calls its slots, so this reaches
/// functions nothing else refers to and shows which function each slot holds.
fn table(m: &mut Module, funcs: &[FunctionId], u: &mut Unstructured) -> Result<()> {
    let size = u.int_in_range(1..=6)?;
    let init = u
        .ratio(2, 3)?
        .then_some(ConstExpr::RefFunc(*u.choose(funcs)?));
    let table = m
        .tables
        .add_local_with_init(false, size, None, RefType::FUNCREF, init);
    m.exports.add("planted_table", table);
    let items = (0..u.int_in_range(0..=size)?)
        .map(|_| u.choose(funcs).copied())
        .collect::<Result<Vec<_>>>()?;
    // Rarely past the end of the table, which fails instantiation.
    let room = size - items.len() as u64;
    let offset = if u.ratio(1, 16)? {
        room + 1
    } else {
        u.int_in_range(0..=room)?
    };
    let segment = m.elements.add(
        ElementKind::Active {
            table,
            offset: ConstExpr::Value(Value::I32(offset as i32)),
        },
        ElementItems::Functions(items),
    );
    m.tables.get_mut(table).elem_segments.insert(segment);
    Ok(())
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
    let ty = *u.choose(&NUMBERS)?;
    let x = m.locals.add(ty);
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let pool: Vec<Value> = (0..u.int_in_range(1..=2)?)
        .map(|_| constant(ty, u))
        .collect::<Result<_>>()?;
    let all = seqs(lf);
    for _ in 0..u.int_in_range(2..=8)? {
        let (s, at) = position(lf, &all, u)?;
        let code = if u.ratio(2, 3)? {
            vec![
                Const {
                    value: *u.choose(&pool)?,
                }
                .into(),
                LocalSet { local: x }.into(),
            ]
        } else {
            let mut value = vec![LocalGet { local: x }.into()];
            value.extend(to_i32(ty));
            observe(g, value)
        };
        insert(lf, s, at, code);
    }
    Ok(())
}

fn tails(m: &mut Module, f: FunctionId, g: GlobalId, u: &mut Unstructured) -> Result<()> {
    let no_results = m.types.get(m.funcs.get(f).ty()).results().is_empty();
    if !matches!(m.funcs.get(f).kind, FunctionKind::Local(_)) {
        return Ok(());
    }
    // Sometimes the tail reads a non-nullable local, written once at the top
    // of the function. Such a read only validates inside the block that
    // holds the write, or one nested in it.
    let reference = if u.ratio(1, 4)? {
        let ty = RefType {
            nullable: false,
            heap_type: HeapType::Abstract(AbstractHeapType::Func),
        };
        m.elements
            .add(ElementKind::Declared, ElementItems::Functions(vec![f]));
        Some(m.locals.add(ValType::Ref(ty)))
    } else {
        None
    };
    let lf = m.funcs.get_mut(f).kind.unwrap_local_mut();
    let conds: Vec<LocalId> = lf
        .args
        .iter()
        .copied()
        .filter(|&a| m.locals.get(a).ty() == ValType::I32)
        .collect();
    // One tail to share, and sometimes a second that differs from it in one
    // constant and so must stay apart.
    let ty = *u.choose(&NUMBERS)?;
    let end: Instr = if no_results && u.arbitrary()? {
        Return {}.into()
    } else {
        Unreachable {}.into()
    };
    let tails: Vec<Vec<Instr>> = (0..u.int_in_range(1..=2)?)
        .map(|_| {
            let mut value = vec![
                Const {
                    value: constant(ty, u)?,
                }
                .into(),
            ];
            value.extend(to_i32(ty));
            if let Some(local) = reference
                && u.arbitrary()?
            {
                value = vec![LocalGet { local }.into(), RefIsNull {}.into()];
            }
            let mut tail = observe(g, value);
            tail.push(end.clone());
            Ok(tail)
        })
        .collect::<Result<_>>()?;
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
        insert(lf, then, 0, u.choose(&tails)?.clone());
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
    // Last, so that the write comes before every tail.
    if let Some(local) = reference {
        let entry = lf.entry_block();
        insert(
            lf,
            entry,
            0,
            [RefFunc { func: f }.into(), LocalSet { local }.into()],
        );
    }
    Ok(())
}

/// `block $out … if $arm … br $out end … end`, the shape `branches` turns
/// into `if … else … end`, sometimes with a way out of the arm or the block
/// from the middle of the arm.
fn arms(m: &mut Module, f: FunctionId, g: GlobalId, u: &mut Unstructured) -> Result<()> {
    let FunctionKind::Local(lf) = &mut m.funcs.get_mut(f).kind else {
        return Ok(());
    };
    let conds: Vec<LocalId> = lf
        .args
        .iter()
        .copied()
        .filter(|&a| m.locals.get(a).ty() == ValType::I32)
        .collect();
    let cond = |u: &mut Unstructured| -> Result<Vec<Instr>> {
        let value = match u.choose(&conds) {
            Ok(&c) => LocalGet { local: c }.into(),
            Err(_) => GlobalGet { global: g }.into(),
        };
        Ok(vec![
            value,
            Const {
                value: Value::I32(u.int_in_range(0..=3)?),
            }
            .into(),
            Binop {
                op: *u.choose(&[BinaryOp::I32And, BinaryOp::I32LtS, BinaryOp::I32Eq])?,
            }
            .into(),
        ])
    };
    let effect = |u: &mut Unstructured| -> Result<Vec<Instr>> {
        Ok(observe(
            g,
            vec![
                Const {
                    value: Value::I32(u.arbitrary()?),
                }
                .into(),
            ],
        ))
    };
    let all = seqs(lf);
    let (s, at) = position(lf, &all, u)?;
    let mut seq = || {
        lf.builder_mut()
            .dangling_instr_seq(InstrSeqType::Simple(None))
            .id()
    };
    let (out, arm, alt) = (seq(), seq(), seq());

    let mut then = effect(u)?;
    match u.int_in_range(0..=3)? {
        0 => then.extend(cond(u)?.into_iter().chain([BrIf { block: arm }.into()])),
        1 => then.extend(cond(u)?.into_iter().chain([BrIf { block: out }.into()])),
        _ => {}
    }
    if u.arbitrary()? {
        then.extend(effect(u)?);
    }
    if u.ratio(7, 8)? {
        then.push(Br { block: out }.into());
    }
    insert(lf, arm, 0, then);

    let mut body = cond(u)?;
    body.push(
        IfElse {
            consequent: arm,
            alternative: alt,
        }
        .into(),
    );
    if u.ratio(7, 8)? {
        body.extend(effect(u)?);
    }
    insert(lf, out, 0, body);
    insert(lf, s, at, [Block { seq: out }.into()]);
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
                *v = match v {
                    Value::I32(_) => constant(ValType::I32, self.u)?,
                    Value::I64(_) => constant(ValType::I64, self.u)?,
                    Value::F32(_) => constant(ValType::F32, self.u)?,
                    Value::F64(_) => constant(ValType::F64, self.u)?,
                    Value::V128(_) => Value::V128(self.u.arbitrary()?),
                };
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
