pub mod eh;
mod plant;

use arbitrary::{Result as ArbResult, Unstructured};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use wasm_shrink::Pass;
use wasmtime::{
    Engine, Extern, GcHeapOutOfMemory, Linker, Store, StoreLimits, StoreLimitsBuilder, Trap, Val,
    ValType,
};

const FUEL: u64 = 1_000_000;

const IMPORTS: &str = r#"(module
  (import "env" "log_i32" (func (param i32)))
  (import "env" "log_i64" (func (param i64)))
  (import "env" "log_f32" (func (param f32)))
  (import "env" "log_f64" (func (param f64)))
  (import "env" "next_i32" (func (result i32)))
  (import "env" "mix" (func (param i32 i64) (result i64))))"#;

pub struct Case {
    pub wasm: Vec<u8>,
    pub passes: Vec<Pass>,
    pub reorder: bool,
    pub args: Vec<u8>,
}

impl Case {
    pub fn generate(data: &[u8]) -> ArbResult<Case> {
        let mut u = Unstructured::new(data);
        let reorder = u.arbitrary()?;
        let passes = if u.ratio(1, 3)? {
            Pass::DEFAULT.to_vec()
        } else {
            let n = u.int_in_range(1..=8)?;
            (0..n)
                .map(|_| u.choose(Pass::ALL).copied())
                .collect::<ArbResult<_>>()?
        };
        let config = smith_config(&mut u)?;
        let mut wasm = wasm_smith::Module::new(config, &mut u)?.to_bytes();
        if u.ratio(3, 4)? {
            wasm = plant::run(&wasm, &mut u)?;
        }
        let args = u.take_rest().to_vec();
        Ok(Case {
            wasm,
            passes,
            reorder,
            args,
        })
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let passes: Vec<&str> = self.passes.iter().map(|p| p.name()).collect();
        write!(f, "--passes {}", passes.join(","))?;
        if !self.reorder {
            write!(f, " --no-reorder")?;
        }
        Ok(())
    }
}

fn smith_config(u: &mut Unstructured) -> ArbResult<wasm_smith::Config> {
    static IMPORTS_WASM: LazyLock<Vec<u8>> = LazyLock::new(|| wat::parse_str(IMPORTS).unwrap());
    Ok(wasm_smith::Config {
        available_imports: Some(IMPORTS_WASM.clone()),
        canonicalize_nans: true,
        export_everything: !u.ratio(1, 4)?,
        min_funcs: 1,
        max_funcs: 40,
        max_instructions: u.int_in_range(10..=400)?,
        max_imports: 6,
        max_memories: 2,
        max_tables: 2,
        max_memory32_bytes: 1 << 20,
        max_memory64_bytes: 1 << 20,
        max_table_elements: 1000,
        gc_enabled: u.arbitrary()?,
        threads_enabled: u.arbitrary()?,
        relaxed_simd_enabled: true,
        ..wasm_smith::Config::default()
    })
}

/// How a case ended when no property failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    Executed,
    Inconclusive,
    Unsupported,
}

pub fn seed_data(seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut next = || {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let len = 256 + (next() % 8192) as usize;
    (0..len).map(|_| next() as u8).collect()
}

pub fn check(data: &[u8]) -> Checked {
    let Ok(case) = Case::generate(data) else {
        return Checked::Unsupported;
    };
    match check_case(&case) {
        Ok(c) => c,
        Err(e) => panic!("{case}\n{e}"),
    }
}

pub fn check_case(case: &Case) -> Result<Checked, String> {
    let Some(out) = shrink(&case.wasm, &case.passes, case.reorder, false) else {
        return Ok(Checked::Unsupported);
    };
    validate(&out.wasm).map_err(|e| format!("output does not validate: {e}"))?;

    let again = shrink(&case.wasm, &case.passes, case.reorder, false).unwrap();
    if again.wasm != out.wasm {
        return Err("output is not deterministic".into());
    }

    let named = shrink(&case.wasm, &case.passes, case.reorder, true).unwrap();
    if sections_without_names(&named.wasm) != sections_without_names(&out.wasm) {
        return Err("names change the output".into());
    }

    if case.reorder {
        check_reorder(&out.wasm)?;
    }

    let Some(expected) = run(&case.wasm, &case.args)? else {
        return Ok(Checked::Unsupported);
    };
    let Some(actual) = run(&out.wasm, &case.args)? else {
        return Ok(Checked::Inconclusive);
    };
    let (Trace::Done(expected), Trace::Done(actual)) = (expected, actual) else {
        return Ok(Checked::Inconclusive);
    };
    if expected != actual {
        let round_trip = shrink(&case.wasm, &[], false, false).unwrap();
        let culprit = match run(&round_trip.wasm, &case.args)? {
            Some(Trace::Done(t)) if t != expected => "the walrus round trip",
            _ => "wasm-shrink",
        };
        return Err(format!(
            "{culprit} changes behaviour\n{}",
            diff(&expected, &actual)
        ));
    }
    Ok(Checked::Executed)
}

pub struct Shrunk {
    pub wasm: Vec<u8>,
    pub saved: u64,
}

pub fn shrink(wasm: &[u8], passes: &[Pass], reorder: bool, names: bool) -> Option<Shrunk> {
    let saved = Arc::new(AtomicU64::new(0));
    let mut config = walrus::ModuleConfig::new();
    if reorder {
        let record = saved.clone();
        config.function_order(move |m, f| {
            record.store(wasm_shrink::most_used_first(m, f), Ordering::Relaxed)
        });
    }
    let mut m = config.parse(wasm).ok()?;
    if names {
        let long = "n".repeat(300);
        for (i, f) in m.funcs.iter_mut().enumerate() {
            f.name = Some(format!("{long}{i}"));
        }
        let locals: Vec<_> = m.locals.iter().map(|l| l.id()).collect();
        for (i, id) in locals.into_iter().enumerate() {
            m.locals.get_mut(id).name = Some(format!("{long}{i}"));
        }
    }
    for &p in passes {
        p.run(&mut m);
    }
    walrus::passes::gc::run(&mut m);
    let wasm = m.emit_wasm();
    Some(Shrunk {
        wasm,
        saved: saved.load(Ordering::Relaxed),
    })
}

fn check_reorder(wasm: &[u8]) -> Result<(), String> {
    let plain = shrink(wasm, &[], false, false).ok_or("walrus rejects its own output")?;
    let once = shrink(wasm, &[], true, false).unwrap();
    let (before, after) = (index_bytes(&plain.wasm), index_bytes(&once.wasm));
    if before != after + once.saved {
        return Err(format!(
            "reorder claims {} bytes saved, but went from {before} to {after}",
            once.saved
        ));
    }
    let twice = shrink(&once.wasm, &[], true, false).unwrap();
    if twice.wasm != once.wasm {
        return Err("reordering again changes the output".into());
    }
    Ok(())
}

fn leb_len(mut v: u64) -> u64 {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn index_bytes(wasm: &[u8]) -> u64 {
    let mut n = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload {
            Ok(wasmparser::Payload::CustomSection(_)) => {}
            Ok(wasmparser::Payload::CodeSectionEntry(body)) => {
                n -= leb_len(body.range().len() as u64);
            }
            Ok(p) => {
                if let Some((_, range)) = p.as_section() {
                    n += range.len() as u64;
                }
            }
            Err(_) => break,
        }
    }
    n
}

pub fn changed_by(wasm: &[u8]) -> Vec<Pass> {
    let Some(base) = shrink(wasm, &[], false, false) else {
        return Vec::new();
    };
    Pass::ALL
        .iter()
        .copied()
        .filter(|&p| shrink(wasm, &[p], false, false).is_some_and(|s| s.wasm != base.wasm))
        .collect()
}

pub fn validate(wasm: &[u8]) -> Result<(), String> {
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(wasm)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) fn sections_without_names(wasm: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let Ok(payload) = payload else { break };
        if let wasmparser::Payload::CustomSection(c) = &payload
            && c.name() == "name"
        {
            continue;
        }
        if let Some((_, range)) = payload.as_section() {
            out.push(&wasm[range]);
        }
    }
    out
}

fn diff(expected: &[String], actual: &[String]) -> String {
    let at = expected
        .iter()
        .zip(actual)
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(actual.len()));
    let from = at.saturating_sub(3);
    let show = |t: &[String]| {
        t[from.min(t.len())..(at + 3).min(t.len())]
            .iter()
            .map(|e| format!("    {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "first difference at event {at}\n  input:\n{}\n  output:\n{}",
        show(expected),
        show(actual)
    )
}

enum Trace {
    Done(Vec<String>),
    Inconclusive,
}

static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    let mut c = wasmtime::Config::new();
    c.consume_fuel(true)
        .wasm_backtrace_max_frames(None)
        .max_wasm_stack(512 << 10)
        .cranelift_opt_level(wasmtime::OptLevel::None)
        .relaxed_simd_deterministic(true)
        .wasm_threads(true)
        .shared_memory(true)
        .wasm_gc(true)
        .wasm_function_references(true)
        .wasm_exceptions(true)
        .wasm_memory64(true)
        .wasm_multi_memory(true)
        .wasm_tail_call(true)
        .wasm_extended_const(true)
        .wasm_relaxed_simd(true);
    Engine::new(&c).unwrap()
});

struct Host {
    events: Vec<String>,
    counter: i32,
    limits: StoreLimits,
}

enum Step {
    Ok,
    Trap(String),
    Error,
    Inconclusive,
}

fn step(r: wasmtime::Result<()>) -> Step {
    match r {
        Ok(()) => Step::Ok,
        Err(e) if e.is::<GcHeapOutOfMemory<()>>() => Step::Inconclusive,
        Err(e) => match e.downcast_ref::<Trap>() {
            Some(Trap::OutOfFuel | Trap::StackOverflow | Trap::AllocationTooLarge) => {
                Step::Inconclusive
            }
            Some(t) => Step::Trap(format!("trap {t:?}")),
            None => Step::Error,
        },
    }
}

fn run(wasm: &[u8], args: &[u8]) -> Result<Option<Trace>, String> {
    let Ok(module) = wasmtime::Module::new(&ENGINE, wasm) else {
        return Ok(None);
    };
    let mut store = Store::new(
        &ENGINE,
        Host {
            events: Vec::new(),
            counter: 0,
            limits: StoreLimitsBuilder::new()
                .memory_size(16 << 20)
                .table_elements(100_000)
                .build(),
        },
    );
    store.limiter(|h| &mut h.limits);
    let mut linker = Linker::new(&ENGINE);
    for import in module.imports() {
        let ty = match import.ty() {
            wasmtime::ExternType::Func(ty) => ty,
            wasmtime::ExternType::Tag(ty) => {
                let Ok(tag) = wasmtime::Tag::new(&mut store, &ty) else {
                    return Ok(None);
                };
                linker
                    .define(&store, import.module(), import.name(), tag)
                    .map_err(|e| e.to_string())?;
                continue;
            }
            _ => return Ok(None),
        };
        let name = import.name().to_string();
        linker
            .func_new(
                import.module(),
                import.name(),
                ty,
                move |mut caller: wasmtime::Caller<'_, Host>, params, results| {
                    let host = caller.data_mut();
                    host.counter = host.counter.wrapping_add(1);
                    let args: Vec<String> = params.iter().map(show_plain).collect();
                    host.events
                        .push(format!("host {name}({})", args.join(", ")));
                    let n = host.counter;
                    for r in results.iter_mut() {
                        match r {
                            Val::I32(x) => *x = n,
                            Val::I64(x) => *x = n as i64 * 0x1_0000_0001,
                            _ => {}
                        }
                    }
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?;
    }
    store.set_fuel(FUEL).unwrap();
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => {
            return Ok(Some(match step(Err(e)) {
                Step::Inconclusive => Trace::Inconclusive,
                Step::Error => return Ok(None),
                Step::Trap(_) => return Ok(None),
                Step::Ok => unreachable!(),
            }));
        }
    };
    let mut trace = std::mem::take(&mut store.data_mut().events);

    let mut u = Unstructured::new(args);
    let exports: Vec<(String, Extern)> = instance
        .exports(&mut store)
        .map(|e| (e.name().to_string(), e.into_extern()))
        .collect();
    for (name, ext) in &exports {
        let Extern::Func(func) = ext else { continue };
        let ty = func.ty(&store);
        let Some(params) = ty
            .params()
            .map(|t| arbitrary_val(&mut u, &t))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut results: Vec<Val> = ty
            .results()
            .map(|t| Val::default_for_ty(&t).unwrap_or(Val::I32(0)))
            .collect();
        store.set_fuel(FUEL).unwrap();
        let r = func.call(&mut store, &params, &mut results);
        trace.append(&mut store.data_mut().events);
        let outcome = match step(r) {
            Step::Inconclusive => return Ok(Some(Trace::Inconclusive)),
            Step::Trap(t) => t,
            Step::Error => "error".into(),
            Step::Ok => {
                let r: Vec<String> = results.iter().map(|v| show(&mut store, v)).collect();
                format!("-> {}", r.join(", "))
            }
        };
        let args: Vec<String> = params.iter().map(show_plain).collect();
        trace.push(format!("call {name}({}) {outcome}", args.join(", ")));
        state(&mut store, &exports, &mut trace);
    }
    Ok(Some(Trace::Done(trace)))
}

fn arbitrary_val(u: &mut Unstructured, ty: &ValType) -> Option<Val> {
    Some(match ty {
        ValType::I32 => Val::I32(u.arbitrary().unwrap_or_default()),
        ValType::I64 => Val::I64(u.arbitrary().unwrap_or_default()),
        ValType::F32 => Val::F32(u.arbitrary().unwrap_or_default()),
        ValType::F64 => Val::F64(u.arbitrary().unwrap_or_default()),
        ValType::V128 => Val::V128(u.arbitrary::<u128>().unwrap_or_default().into()),
        ValType::Ref(_) => Val::default_for_ty(ty)?,
    })
}

fn show_plain(v: &Val) -> String {
    match v {
        Val::I32(x) => format!("i32 {x}"),
        Val::I64(x) => format!("i64 {x}"),
        Val::F32(x) => format!("f32 {x:#x}"),
        Val::F64(x) => format!("f64 {x:#x}"),
        Val::V128(x) => format!("v128 {:#x}", x.as_u128()),
        _ => "ref".into(),
    }
}

fn show(store: &mut Store<Host>, v: &Val) -> String {
    match v {
        Val::FuncRef(f) => if f.is_some() { "funcref" } else { "null" }.into(),
        Val::ExternRef(r) => if r.is_some() { "externref" } else { "null" }.into(),
        Val::AnyRef(None) | Val::ExnRef(None) | Val::ContRef(None) => "null".into(),
        Val::AnyRef(Some(r)) => match r.as_i31(&mut *store) {
            Ok(Some(i)) => format!("i31 {}", i.get_u32()),
            _ => "anyref".into(),
        },
        Val::ExnRef(Some(_)) => "exnref".into(),
        Val::ContRef(Some(_)) => "contref".into(),
        v => show_plain(v),
    }
}

fn state(store: &mut Store<Host>, exports: &[(String, Extern)], trace: &mut Vec<String>) {
    for (name, ext) in exports {
        match ext {
            Extern::Memory(m) => {
                let data = m.data(&*store);
                trace.push(format!(
                    "memory {name}: {} bytes, {:#x}",
                    data.len(),
                    hash(data)
                ));
            }
            Extern::SharedMemory(m) => {
                let data: Vec<u8> = m.data().iter().map(|b| unsafe { *b.get() }).collect();
                trace.push(format!(
                    "memory {name}: {} bytes, {:#x}",
                    data.len(),
                    hash(&data)
                ));
            }
            Extern::Global(g) => {
                let v = g.get(&mut *store);
                let v = show(store, &v);
                trace.push(format!("global {name}: {v}"));
            }
            Extern::Table(t) => {
                let size = t.size(&*store);
                let nulls: String = (0..size.min(1000))
                    .map(|i| match t.get(&mut *store, i) {
                        Some(r) if r.is_null() => '0',
                        _ => '1',
                    })
                    .collect();
                trace.push(format!("table {name}: {size} {nulls}"));
            }
            _ => {}
        }
    }
}

fn hash(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}
