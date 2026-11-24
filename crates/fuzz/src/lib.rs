pub mod eh;
mod plant;

use arbitrary::{Result as ArbResult, Unstructured};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use wasm_shrink::Pass;
use wasmtime::{
    Engine, Extern, Func, GcHeapOutOfMemory, Linker, Store, StoreLimits, StoreLimitsBuilder, Tag,
    Trap, Val, ValType,
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
    // An invalid input would only show up as a case that cannot run.
    validate(&case.wasm).map_err(|e| format!("input does not validate: {e}"))?;
    let Some(out) = shrink(&case.wasm, &case.passes, case.reorder, false) else {
        return Ok(Checked::Unsupported);
    };
    validate(&out.wasm).map_err(|e| format!("output does not validate: {e}"))?;
    if opaque_sections(&case.wasm) != opaque_sections(&out.wasm) {
        return Err("custom section contents changed".into());
    }

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

    if never_grows(&case.passes) {
        let plain = shrink(&case.wasm, &[], case.reorder, false).unwrap();
        let (before, after) = (shipped_bytes(&plain.wasm), shipped_bytes(&out.wasm));
        if after > before {
            return Err(format!(
                "output is {} bytes larger than the input ({before} -> {after})",
                after - before
            ));
        }
    }

    // A call that runs out of fuel or stack says nothing, and leaves the
    // instance in a state the output need not reach. Run the input again
    // without each such call, then the output without the same calls.
    let mut skip = HashSet::new();
    let expected = loop {
        let Some(trace) = run(&case.wasm, &case.args, &skip)? else {
            return Ok(Checked::Unsupported);
        };
        match trace.end {
            End::Call(k) if skip.len() < MAX_SKIPS => skip.insert(k),
            _ => break trace,
        };
    };
    let Some(actual) = run(&out.wasm, &case.args, &skip)? else {
        return Err("runtime accepts input but cannot run optimized module".into());
    };
    // A run that still stopped early has to agree with the other one up to
    // where it stopped.
    let complete = expected.end == End::Done && actual.end == End::Done;
    if let Some(at) = first_difference(&expected, &actual) {
        let round_trip = shrink(&case.wasm, &[], false, false).unwrap();
        let culprit = match run(&round_trip.wasm, &case.args, &skip)? {
            Some(t) if first_difference(&expected, &t).is_some() => "the walrus round trip",
            _ => "wasm-shrink",
        };
        return Err(format!(
            "{culprit} changes behaviour\n{}",
            diff(&expected.events, &actual.events, at)
        ));
    }
    Ok(if complete {
        Checked::Executed
    } else {
        Checked::Inconclusive
    })
}

/// The index of the first event the two runs disagree on.
fn first_difference(expected: &Trace, actual: &Trace) -> Option<usize> {
    let mut ids = HashMap::new();
    let shared = expected.events.len().min(actual.events.len());
    (0..shared)
        .find(|&k| !same_event(&expected.events[k], &actual.events[k], &mut ids))
        .or_else(|| {
            let complete = expected.end == End::Done && actual.end == End::Done;
            (complete && expected.events.len() != actual.events.len()).then_some(shared)
        })
}

/// Stands in front of a function's identity number in an event. Names are
/// printed escaped, so it only ever comes from [`show`].
const FUNC: char = '\u{1}';

/// Whether two events are the same up to function identity. Passes merge
/// equivalent functions, so two functions of the input may be one function in
/// the output, but one function never becomes two: `ids` maps each input
/// identity to the output identity it had the first time.
fn same_event<'a>(expected: &'a str, actual: &'a str, ids: &mut HashMap<&'a str, &'a str>) -> bool {
    let id = |s: &'a str| s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let (mut e, mut a) = (expected.split(FUNC), actual.split(FUNC));
    if e.next() != a.next() {
        return false;
    }
    loop {
        match (e.next(), a.next()) {
            (None, None) => return true,
            (Some(e), Some(a)) => {
                let ((e, e_rest), (a, a_rest)) = (id(e), id(a));
                if e_rest != a_rest || *ids.entry(e).or_insert(a) != a {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

pub struct Shrunk {
    pub wasm: Vec<u8>,
    pub saved: u64,
}

fn opaque_sections(wasm: &[u8]) -> Vec<(&str, &[u8])> {
    let mut sections: Vec<_> = wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p.ok()? {
            wasmparser::Payload::CustomSection(s)
                if !matches!(s.name(), "name" | "producers") && !s.name().starts_with(".debug") =>
            {
                Some((s.name(), s.data()))
            }
            _ => None,
        })
        .collect();
    sections.sort_by_key(|s| s.0);
    sections
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
    wasm_shrink::gc(&mut m);
    for &p in passes {
        p.run(&mut m);
    }
    wasm_shrink::gc(&mut m);
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

/// The size of `wasm` without its custom sections.
/// Whether the passes promise an output no larger than their input. Every
/// pass is there to make the module smaller, with two exceptions:
/// `translate-to-exnref` changes the encoding, not the size, and `inline`
/// counts on a later `branches` to remove the `br`s that it makes of a
/// callee's `return`s.
fn never_grows(passes: &[Pass]) -> bool {
    let last = |pass| passes.iter().rposition(|&p| p == pass);
    !passes.contains(&Pass::TranslateToExnref) && last(Pass::Inline) <= last(Pass::Branches)
}

fn shipped_bytes(wasm: &[u8]) -> usize {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p.ok()? {
            wasmparser::Payload::CustomSection(_) => None,
            p => Some(p.as_section()?.1.len()),
        })
        .sum()
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

fn diff(expected: &[String], actual: &[String], at: usize) -> String {
    let from = at.saturating_sub(3);
    let show = |t: &[String]| {
        t[from.min(t.len())..(at + 3).min(t.len())]
            .iter()
            .map(|e| format!("    {}", e.replace(FUNC, "func#")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "first difference at event {at}\n  input:\n{}\n  output:\n{}",
        show(expected),
        show(actual)
    )
}

/// What a module did, and where the run stopped.
struct Trace {
    events: Vec<String>,
    end: End,
}

#[derive(PartialEq)]
enum End {
    Done,
    /// Instantiation ran out of fuel or stack.
    Instantiation,
    /// So did the call with this index.
    Call(usize),
}

/// How many calls of one case can be left out for running out of fuel.
const MAX_SKIPS: usize = 8;

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
    /// Functions seen so far, numbered in the order [`show`] met them.
    funcs: HashMap<usize, usize>,
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

/// Instantiate `wasm` and call its exports, except the calls in `skip`.
fn run(wasm: &[u8], args: &[u8], skip: &HashSet<usize>) -> Result<Option<Trace>, String> {
    let Ok(module) = wasmtime::Module::new(&ENGINE, wasm) else {
        return Ok(None);
    };
    let mut store = Store::new(
        &ENGINE,
        Host {
            events: Vec::new(),
            funcs: Default::default(),
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
                        .push(format!("host {name:?}({})", args.join(", ")));
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
            let mut events = std::mem::take(&mut store.data_mut().events);
            let end = match step(Err(e)) {
                Step::Inconclusive => End::Instantiation,
                Step::Trap(t) => {
                    events.push(format!("instantiation: {t}"));
                    End::Done
                }
                Step::Error => {
                    events.push(format!("instantiation: {}", error(&mut store, &[])));
                    End::Done
                }
                Step::Ok => unreachable!(),
            };
            return Ok(Some(Trace { events, end }));
        }
    };
    let mut events = std::mem::take(&mut store.data_mut().events);
    events.push("instantiated".into());

    let mut exports: Vec<(String, Extern)> = instance
        .exports(&mut store)
        .map(|e| (e.name().to_string(), e.into_extern()))
        .collect();
    exports.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, ext) in &exports {
        if let Extern::Func(f) = ext {
            let f = show(&mut store, &Val::FuncRef(Some(*f)));
            events.push(format!("export {name:?}: {f}"));
        }
    }
    state(&mut store, &exports, &mut events);

    let mut u = Unstructured::new(args);
    let mut calls: Vec<(String, Func, Vec<Val>)> = Vec::new();
    for (name, ext) in &exports {
        let Extern::Func(func) = ext else { continue };
        let ty = func.ty(&store);
        let Some(random) = ty
            .params()
            .map(|t| arbitrary_val(&mut u, &t))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        // Zero and one take both arms of most conditions, whatever the
        // random arguments were.
        let fixed = [0, 1].map(|n| probe(&ty, n)).into_iter().flatten();
        let probes = if random.is_empty() { None } else { Some(fixed) };
        calls.push((format!("{name:?}"), *func, random));
        for params in probes.into_iter().flatten() {
            calls.push((format!("{name:?}"), *func, params));
        }
    }
    // A function can be reachable through a table alone.
    for (name, ext) in &exports {
        let Extern::Table(table) = ext else { continue };
        for slot in 0..table.size(&store).min(TABLE_CALLS) {
            let Some(wasmtime::Ref::Func(Some(func))) = table.get(&mut store, slot) else {
                continue;
            };
            if let Some(params) = probe(&func.ty(&store), slot as i32) {
                calls.push((format!("{name:?}[{slot}]"), func, params));
            }
        }
    }
    for (index, (name, func, params)) in calls.into_iter().enumerate() {
        let args: Vec<String> = params.iter().map(show_plain).collect();
        if skip.contains(&index) {
            events.push(format!("call {name}({}) skipped", args.join(", ")));
            continue;
        }
        let mut results: Vec<Val> = func
            .ty(&store)
            .results()
            .map(|t| Val::default_for_ty(&t).unwrap_or(Val::I32(0)))
            .collect();
        store.set_fuel(FUEL).unwrap();
        let r = func.call(&mut store, &params, &mut results);
        events.append(&mut store.data_mut().events);
        let outcome = match step(r) {
            Step::Inconclusive => {
                return Ok(Some(Trace {
                    events,
                    end: End::Call(index),
                }));
            }
            Step::Trap(t) => t,
            Step::Error => error(&mut store, &exports),
            Step::Ok => {
                let r: Vec<String> = results.iter().map(|v| show(&mut store, v)).collect();
                format!("-> {}", r.join(", "))
            }
        };
        events.push(format!("call {name}({}) {outcome}", args.join(", ")));
        state(&mut store, &exports, &mut events);
    }
    Ok(Some(Trace {
        events,
        end: End::Done,
    }))
}

/// How many slots of each exported table get called.
const TABLE_CALLS: u64 = 16;

/// Arguments for a function of type `ty`: `n` for every integer, the default
/// for everything else. `None` if a parameter has no default.
fn probe(ty: &wasmtime::FuncType, n: i32) -> Option<Vec<Val>> {
    ty.params()
        .map(|t| match t {
            ValType::I32 => Some(Val::I32(n)),
            ValType::I64 => Some(Val::I64(n.into())),
            _ => Val::default_for_ty(&t),
        })
        .collect()
}

/// A failure that is not a trap: an exception nothing caught, with its tag
/// (by export name) and payload.
fn error(store: &mut Store<Host>, exports: &[(String, Extern)]) -> String {
    let Some(exn) = store.take_pending_exception() else {
        return "error".into();
    };
    let Ok(tag) = exn.tag(&mut *store) else {
        return "throw".into();
    };
    let name = exports.iter().find_map(|(name, e)| match e {
        Extern::Tag(t) if Tag::eq(t, &tag, &*store) => Some(format!("{name:?}")),
        _ => None,
    });
    let count = tag.ty(&*store).ty().params().len();
    let fields: Vec<String> = (0..count)
        .map(|i| match exn.field(&mut *store, i) {
            Ok(v) => show(store, &v),
            Err(_) => "?".into(),
        })
        .collect();
    format!(
        "throw {}({})",
        name.as_deref().unwrap_or("an unexported tag"),
        fields.join(", ")
    )
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
        Val::FuncRef(Some(f)) => {
            let raw = f.to_raw(&mut *store).addr();
            let ids = &mut store.data_mut().funcs;
            let next = ids.len();
            format!("{FUNC}{}", ids.entry(raw).or_insert(next))
        }
        Val::FuncRef(None) => "null".into(),
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
                    "memory {name:?}: {} bytes, {:#x}",
                    data.len(),
                    hash(data)
                ));
            }
            Extern::SharedMemory(m) => {
                let data = m.data();
                let bytes = data
                    .iter()
                    .map(|b| unsafe { AtomicU8::from_ptr(b.get()) }.load(Ordering::Relaxed));
                trace.push(format!(
                    "memory {name:?}: {} bytes, {:#x}",
                    data.len(),
                    hash_bytes(bytes)
                ));
            }
            Extern::Global(g) => {
                let v = g.get(&mut *store);
                let v = show(store, &v);
                trace.push(format!("global {name:?}: {v}"));
            }
            Extern::Table(t) => {
                let size = t.size(&*store);
                let entries: Vec<_> = (0..size.min(1000))
                    .map(|i| {
                        let value = t.get(&mut *store, i).unwrap();
                        show(store, &value.into())
                    })
                    .collect();
                trace.push(format!("table {name:?}: {size} [{}]", entries.join(", ")));
            }
            _ => {}
        }
    }
}

fn hash(data: &[u8]) -> u64 {
    hash_bytes(data.iter().copied())
}

fn hash_bytes(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::{FUNC, same_event};
    use std::collections::HashMap;

    #[test]
    fn functions_may_merge_but_not_split() {
        let event = |ids: &[u32]| {
            let ids: Vec<String> = ids.iter().map(|i| format!("{FUNC}{i}")).collect();
            format!("table: {}", ids.join(", "))
        };
        let (two, one, other) = (event(&[0, 1]), event(&[0, 0]), event(&[1, 1]));
        let mut ids = HashMap::new();
        // Two functions became one.
        assert!(same_event(&two, &one, &mut ids));
        // Still the same one the next time they show up.
        assert!(same_event(&two, &one, &mut ids));
        assert!(!same_event(&two, &other, &mut ids));
        // One function did not become two.
        assert!(!same_event(&one, &two, &mut HashMap::new()));
        assert!(!same_event(&one, "table: 0, 0", &mut HashMap::new()));
    }
}
