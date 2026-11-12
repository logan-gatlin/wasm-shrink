//! Size-driven inlining.
//!
//! A function is inlined into all of its call sites, and then deleted, when that
//! makes the module smaller by the byte estimate below. That covers:
//! - functions with one call site (the common case), which cost a function
//!   entry, a code-section header, a local declaration and a name to keep apart;
//! - tiny functions whose body is about the size of the `call` to them.
//!
//! This is the size part of binaryen's `inlining-optimizing` at -Os/-Oz, without
//! the re-optimization binaryen runs on the result.
//!
//! The callee body is copied into the caller:
//!
//! ```text
//! local.set $argN-1 .. local.set $arg0     ;; pop the call's operands
//! (zero the callee's other locals if needed)
//! block (result ..) <body> end             ;; `return` becomes `br` to it
//! ```
//!
//! The block is dropped when nothing branches to it.
//!
//! Wasm zeroes locals on function entry, and callee code may rely on that. The
//! fresh caller locals are zero the first time the site runs, so they only need
//! explicit zeroing when the site is inside a loop, and then only for locals the
//! callee might read before writing.

use crate::util::{Uses, function_uses, leb_len, par_map};
use std::collections::{HashMap, HashSet};
use walrus::ir::*;
use walrus::{FunctionId, LocalFunction, LocalId, Module, ValType};

#[derive(Default, Debug)]
pub struct Stats {
    pub inlined_functions: usize,
    pub inlined_sites: usize,
    pub rounds: usize,
}

/// An owned copy of a callee body.
pub(crate) struct Snapshot {
    pub entry: InstrSeqId,
    pub seqs: HashMap<InstrSeqId, (InstrSeqType, Vec<Instr>)>,
    pub args: Vec<LocalId>,
    /// Non-argument locals, in first-use order.
    pub locals: Vec<LocalId>,
    /// Locals that may be read before they are written.
    maybe_uninit: HashSet<LocalId>,
    /// Some instruction branches to (or returns from) the entry block.
    branches_to_entry: bool,
    /// Locals the body writes (`local.set`/`local.tee`).
    written: HashSet<LocalId>,
}

pub(crate) fn children(instr: &Instr) -> Vec<InstrSeqId> {
    match instr {
        Instr::Block(b) => vec![b.seq],
        Instr::Loop(l) => vec![l.seq],
        Instr::IfElse(i) => vec![i.consequent, i.alternative],
        Instr::TryTable(t) => vec![t.seq],
        Instr::Try(t) => {
            let mut v = vec![t.seq];
            for c in &t.catches {
                match c {
                    LegacyCatch::Catch { handler, .. } | LegacyCatch::CatchAll { handler } => {
                        v.push(*handler)
                    }
                    LegacyCatch::Delegate { .. } => {}
                }
            }
            v
        }
        _ => vec![],
    }
}

fn branch_targets(instr: &Instr) -> Vec<InstrSeqId> {
    match instr {
        Instr::Br(b) => vec![b.block],
        Instr::BrIf(b) => vec![b.block],
        Instr::BrTable(b) => b.blocks.iter().copied().chain([b.default]).collect(),
        Instr::BrOnNull(b) => vec![b.block],
        Instr::BrOnNonNull(b) => vec![b.block],
        Instr::BrOnCast(b) => vec![b.block],
        Instr::BrOnCastFail(b) => vec![b.block],
        Instr::TryTable(t) => t
            .catches
            .iter()
            .map(|c| match c {
                TryTableCatch::Catch { label, .. }
                | TryTableCatch::CatchRef { label, .. }
                | TryTableCatch::CatchAll { label }
                | TryTableCatch::CatchAllRef { label } => *label,
            })
            .collect(),
        _ => vec![],
    }
}

fn local_of(instr: &Instr) -> Option<LocalId> {
    match instr {
        Instr::LocalGet(g) => Some(g.local),
        Instr::LocalSet(s) => Some(s.local),
        Instr::LocalTee(t) => Some(t.local),
        _ => None,
    }
}

/// Reasons a function body is never inlined.
pub(crate) fn inlinable(f: &LocalFunction) -> bool {
    let mut ok = true;
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        for (i, _) in &f.block(s).instrs {
            if matches!(
                i,
                Instr::ReturnCall(_) | Instr::ReturnCallIndirect(_) | Instr::ReturnCallRef(_)
            ) {
                ok = false;
            }
            stack.extend(children(i));
        }
    }
    ok
}

impl Snapshot {
    pub(crate) fn take(m: &Module, f: &LocalFunction) -> Snapshot {
        let entry = f.entry_block();
        let mut seqs = HashMap::new();
        let mut order = Vec::new();
        let mut stack = vec![entry];
        let mut branches_to_entry = false;
        while let Some(s) = stack.pop() {
            let seq = f.block(s);
            let instrs: Vec<Instr> = seq.instrs.iter().map(|(i, _)| i.clone()).collect();
            for i in &instrs {
                stack.extend(children(i));
                if matches!(i, Instr::Return(_)) || branch_targets(i).contains(&entry) {
                    branches_to_entry = true;
                }
            }
            seqs.insert(s, (seq.ty, instrs));
            order.push(s);
        }
        // Locals in textual order, and which ones are written before any read
        // on every path: a write in the entry block's top level, before any
        // nested control flow that touches the local.
        let args: HashSet<LocalId> = f.args.iter().copied().collect();
        let mut locals = Vec::new();
        let mut seen: HashSet<LocalId> = HashSet::new();
        fn walk(
            seqs: &HashMap<InstrSeqId, (InstrSeqType, Vec<Instr>)>,
            s: InstrSeqId,
            out: &mut Vec<LocalId>,
        ) {
            for i in &seqs[&s].1 {
                if let Some(l) = local_of(i) {
                    out.push(l);
                }
                for c in children(i) {
                    walk(seqs, c, out);
                }
            }
        }
        let mut all = Vec::new();
        walk(&seqs, entry, &mut all);
        for l in all {
            if !args.contains(&l) && seen.insert(l) {
                locals.push(l);
            }
        }
        let mut assigned: HashSet<LocalId> = HashSet::new();
        let mut maybe_uninit: HashSet<LocalId> = HashSet::new();
        for i in &seqs[&entry].1 {
            match i {
                Instr::LocalSet(s) => {
                    assigned.insert(s.local);
                }
                Instr::LocalTee(t) => {
                    assigned.insert(t.local);
                }
                Instr::LocalGet(g) if !assigned.contains(&g.local) => {
                    maybe_uninit.insert(g.local);
                }
                _ => {
                    for c in children(i) {
                        let mut inner = Vec::new();
                        walk(&seqs, c, &mut inner);
                        for l in inner {
                            if !assigned.contains(&l) {
                                maybe_uninit.insert(l);
                            }
                        }
                    }
                }
            }
        }
        maybe_uninit.retain(|l| !args.contains(l));
        let _ = (m, order);
        let written = seqs
            .values()
            .flat_map(|(_, is)| is.iter())
            .filter_map(|i| match i {
                Instr::LocalSet(x) => Some(x.local),
                Instr::LocalTee(x) => Some(x.local),
                _ => None,
            })
            .collect();
        Snapshot {
            entry,
            seqs,
            args: f.args.clone(),
            locals,
            maybe_uninit,
            branches_to_entry,
            written,
        }
    }
}

fn zero_bytes(ty: ValType) -> Option<usize> {
    Some(match ty {
        ValType::I32 | ValType::I64 => 2,
        ValType::F32 => 5,
        ValType::F64 => 9,
        ValType::V128 => 18,
        ValType::Ref(r) if r.nullable => 2,
        ValType::Ref(_) => return None,
    })
}

fn zero_value(ty: ValType) -> Instr {
    match ty {
        ValType::I32 => Const {
            value: Value::I32(0),
        }
        .into(),
        ValType::I64 => Const {
            value: Value::I64(0),
        }
        .into(),
        ValType::F32 => Const {
            value: Value::F32(0.0),
        }
        .into(),
        ValType::F64 => Const {
            value: Value::F64(0.0),
        }
        .into(),
        ValType::V128 => Const {
            value: Value::V128(0),
        }
        .into(),
        ValType::Ref(r) => RefNull { ty: r }.into(),
    }
}

fn knob(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Estimated size change (bytes) of inlining `g` everywhere and deleting it.
///
/// `size` is (instruction bytes, body bytes) from [`crate::size::function`].
/// Without a snapshot this is an upper bound: it assumes no wrapper block and
/// no zeroing of locals, which can only lower the gain. It is used to skip
/// copying the bodies of functions that could never pay.
#[allow(clippy::too_many_arguments)]
fn gain(
    m: &Module,
    g: FunctionId,
    nargs: usize,
    snap: Option<&Snapshot>,
    uses: &Uses,
    (instr_bytes, body_len): (usize, usize),
    call_leb: usize,
    in_loop_sites: usize,
) -> Option<i64> {
    let call = 1 + call_leb as i64;
    let n = uses.call_count() as i64;
    // Per site: arguments popped into locals, the body, a block around it.
    let mut site = instr_bytes as i64 + 2 * nargs as i64;
    let mut zero = 0i64;
    if let Some(snap) = snap {
        if snap.branches_to_entry {
            site += 3;
        }
        for l in &snap.maybe_uninit {
            zero += zero_bytes(m.locals.get(*l).ty())? as i64 + 2;
        }
    }
    // Removing a function also removes its name-section entry. Counting that
    // for a function with one call site is fair (the body moves, it does not
    // grow); counting it for several sites would trade code for names, which
    // stops paying the moment names are stripped. WS_INLINE_NAMES=2 allows it.
    let names_mode = knob("WS_INLINE_NAMES", 1);
    let name = if names_mode == 0 || (names_mode == 1 && n > 1) {
        0
    } else {
        m.funcs
            .get(g)
            .name
            .as_ref()
            .map_or(0, |s| call_leb + leb_len(s.len() as u64) + s.len()) as i64
    };
    if n > knob("WS_INLINE_MAX_SITES", i64::MAX) {
        return None;
    }
    // Function-section entry, code-section length prefix, the body, the name.
    let removed = 1 + leb_len(body_len as u64) as i64 + body_len as i64 + name;
    Some(removed + n * call - n * site - in_loop_sites as i64 * zero)
}

/// A direct call site: (caller, sequence, index, inside a loop).
type Site = (FunctionId, InstrSeqId, usize, bool);

/// Every direct call site in the module, by callee, in one walk.
fn all_sites(m: &Module) -> HashMap<FunctionId, Vec<Site>> {
    // Walk callers in parallel, then merge in function order so each list is
    // in the same order as a sequential walk.
    let per_caller = par_map(m, |caller, f| {
        let mut sites = Vec::new();
        let mut stack = vec![(f.entry_block(), false)];
        while let Some((s, in_loop)) = stack.pop() {
            for (k, (i, _)) in f.block(s).instrs.iter().enumerate() {
                if let Instr::Call(c) = i {
                    sites.push((c.func, (caller, s, k, in_loop)));
                }
                let is_loop = matches!(i, Instr::Loop(_));
                for c in children(i) {
                    stack.push((c, in_loop || is_loop));
                }
            }
        }
        sites
    });
    let mut out: HashMap<FunctionId, Vec<Site>> = HashMap::new();
    for (_, sites) in per_caller {
        for (g, site) in sites {
            out.entry(g).or_default().push(site);
        }
    }
    out
}

pub(crate) fn copy_seq(
    snap: &Snapshot,
    src: InstrSeqId,
    dst: InstrSeqId,
    b: &mut walrus::FunctionBuilder,
    seq_map: &mut HashMap<InstrSeqId, InstrSeqId>,
    locals: &HashMap<LocalId, LocalId>,
    wrapper: InstrSeqId,
) {
    let mut out = Vec::with_capacity(snap.seqs[&src].1.len());
    for instr in &snap.seqs[&src].1 {
        let mut i = instr.clone();
        let fresh =
            |id: InstrSeqId, b: &mut walrus::FunctionBuilder, seq_map: &mut HashMap<_, _>| {
                let n = b.dangling_instr_seq(snap.seqs[&id].0).id();
                seq_map.insert(id, n);
                n
            };
        // Allocate nested sequences first so branches inside them can resolve.
        match &mut i {
            Instr::Block(x) => x.seq = fresh(x.seq, b, seq_map),
            Instr::Loop(x) => x.seq = fresh(x.seq, b, seq_map),
            Instr::IfElse(x) => {
                x.consequent = fresh(x.consequent, b, seq_map);
                x.alternative = fresh(x.alternative, b, seq_map);
            }
            Instr::TryTable(x) => {
                x.seq = fresh(x.seq, b, seq_map);
                for c in &mut x.catches {
                    match c {
                        TryTableCatch::Catch { label, .. }
                        | TryTableCatch::CatchRef { label, .. }
                        | TryTableCatch::CatchAll { label }
                        | TryTableCatch::CatchAllRef { label } => *label = seq_map[label],
                    }
                }
            }
            Instr::Try(x) => {
                x.seq = fresh(x.seq, b, seq_map);
                for c in &mut x.catches {
                    match c {
                        LegacyCatch::Catch { handler, .. } | LegacyCatch::CatchAll { handler } => {
                            *handler = fresh(*handler, b, seq_map)
                        }
                        LegacyCatch::Delegate { .. } => {}
                    }
                }
            }
            Instr::Br(x) => x.block = seq_map[&x.block],
            Instr::BrIf(x) => x.block = seq_map[&x.block],
            Instr::BrTable(x) => {
                x.blocks = x.blocks.iter().map(|s| seq_map[s]).collect();
                x.default = seq_map[&x.default];
            }
            Instr::BrOnNull(x) => x.block = seq_map[&x.block],
            Instr::BrOnNonNull(x) => x.block = seq_map[&x.block],
            Instr::BrOnCast(x) => x.block = seq_map[&x.block],
            Instr::BrOnCastFail(x) => x.block = seq_map[&x.block],
            Instr::Return(_) => i = Br { block: wrapper }.into(),
            Instr::LocalGet(x) => x.local = locals[&x.local],
            Instr::LocalSet(x) => x.local = locals[&x.local],
            Instr::LocalTee(x) => x.local = locals[&x.local],
            _ => {}
        }
        // Fill nested sequences (their ids are now in `seq_map`).
        for (old, new) in children(instr).into_iter().zip(children(&i)) {
            copy_seq(snap, old, new, b, seq_map, locals, wrapper);
        }
        out.push(i);
    }
    b.instr_seq(dst)
        .instrs_mut()
        .extend(out.into_iter().map(|i| (i, InstrLocId::default())));
}

/// Inline `snap` at the call at `seq[idx]` in `caller`.
fn inline_site(
    m: &mut Module,
    caller: FunctionId,
    seq: InstrSeqId,
    idx: usize,
    in_loop: bool,
    snap: &Snapshot,
) {
    {
        // Arguments passed as a plain `local.get $x` to a parameter the callee
        // never writes need no copy: the body reads `$x` directly. (Nothing in the
        // inlined body can write `$x`; it only touches its own fresh locals.)
        let mut forwarded: HashMap<LocalId, LocalId> = HashMap::new();
        let mut first_arg = idx;
        {
            let caller_seq = &m.funcs.get(caller).kind.unwrap_local().block(seq).instrs;
            let mut k = snap.args.len();
            while k > 0 && first_arg > 0 {
                let p = snap.args[k - 1];
                match &caller_seq[first_arg - 1].0 {
                    Instr::LocalGet(g) if !snap.written.contains(&p) => {
                        forwarded.insert(p, g.local);
                        k -= 1;
                        first_arg -= 1;
                    }
                    _ => break,
                }
            }
        }
        let mut locals = HashMap::new();
        for &l in snap.args.iter().chain(&snap.locals) {
            let mapped = match forwarded.get(&l) {
                Some(&x) => x,
                None => m.locals.add(m.locals.get(l).ty()),
            };
            locals.insert(l, mapped);
        }
        let zero: Vec<(LocalId, ValType)> = if in_loop {
            snap.locals
                .iter()
                .filter(|l| snap.maybe_uninit.contains(l))
                .map(|&l| (locals[&l], m.locals.get(l).ty()))
                .collect()
        } else {
            Vec::new()
        };
        // A function's entry type is internal to walrus and never emitted, so a
        // multi-value block needs a regular type.
        let entry_ty = match snap.seqs[&snap.entry].0 {
            InstrSeqType::MultiValue(t) => {
                let results = m.types.get(t).results().to_vec();
                InstrSeqType::new(&mut m.types, &[], &results)
            }
            simple => simple,
        };
        let f = m.funcs.get_mut(caller).kind.unwrap_local_mut();
        let b = f.builder_mut();
        let wrapper = b.dangling_instr_seq(entry_ty).id();
        let mut seq_map = HashMap::new();
        seq_map.insert(snap.entry, wrapper);
        copy_seq(snap, snap.entry, wrapper, b, &mut seq_map, &locals, wrapper);

        let mut repl: Vec<Instr> = Vec::new();
        for a in snap
            .args
            .iter()
            .rev()
            .filter(|a| !forwarded.contains_key(a))
        {
            repl.push(LocalSet { local: locals[a] }.into());
        }
        for (l, ty) in zero {
            repl.push(zero_value(ty));
            repl.push(LocalSet { local: l }.into());
        }
        if snap.branches_to_entry {
            repl.push(Block { seq: wrapper }.into());
        } else {
            let body = std::mem::take(b.instr_seq(wrapper).instrs_mut());
            repl.extend(body.into_iter().map(|(i, _)| i));
        }
        f.block_mut(seq).instrs.splice(
            first_arg..idx + 1,
            repl.into_iter().map(|i| (i, InstrLocId::default())),
        );
    }
}

/// Candidates in callee-first order, so that a function is inlined only after
/// its own inlinable callees have been inlined into it. Functions on a call
/// cycle among the candidates are dropped: a callee-first order does not exist
/// for them.
fn callee_first(
    cands: &[FunctionId],
    site_map: &HashMap<FunctionId, Vec<Site>>,
) -> Vec<FunctionId> {
    let set: HashSet<FunctionId> = cands.iter().copied().collect();
    // caller -> candidate callees, in a fixed order.
    let mut callees: HashMap<FunctionId, Vec<FunctionId>> = HashMap::new();
    for &g in cands {
        for site in &site_map[&g] {
            if set.contains(&site.0) {
                let v = callees.entry(site.0).or_default();
                if !v.contains(&g) {
                    v.push(g);
                }
            }
        }
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let mut mark: HashMap<FunctionId, Mark> = cands.iter().map(|&g| (g, Mark::New)).collect();
    let mut cyclic: HashSet<FunctionId> = HashSet::new();
    let mut order = Vec::new();
    for &root in cands {
        if mark[&root] != Mark::New {
            continue;
        }
        // Iterative DFS: (node, next callee index).
        let mut stack: Vec<(FunctionId, usize)> = vec![(root, 0)];
        mark.insert(root, Mark::Open);
        while let Some(&mut (g, ref mut k)) = stack.last_mut() {
            let next = callees.get(&g).and_then(|v| v.get(*k)).copied();
            *k += 1;
            match next {
                Some(h) => match mark[&h] {
                    Mark::New => {
                        mark.insert(h, Mark::Open);
                        stack.push((h, 0));
                    }
                    Mark::Open => {
                        // Back edge: everything on the stack from `h` up is on a cycle.
                        let from = stack.iter().position(|e| e.0 == h).unwrap();
                        cyclic.extend(stack[from..].iter().map(|e| e.0));
                    }
                    Mark::Done => {}
                },
                None => {
                    mark.insert(g, Mark::Done);
                    order.push(g);
                    stack.pop();
                }
            }
        }
    }
    order.retain(|g| !cyclic.contains(g));
    order
}

pub fn run(m: &mut Module, max_rounds: usize) -> Stats {
    let mut stats = Stats::default();
    for _ in 0..max_rounds {
        stats.rounds += 1;
        let uses = function_uses(m);
        let site_map = all_sites(m);
        let call_leb = leb_len(m.funcs.iter().count() as u64);
        // Candidates, by the snapshot-free upper bound on the gain.
        let cands: Vec<FunctionId> = par_map(m, |g, f| {
            let u = uses.get(&g)?;
            if u.pinned || u.calls.is_empty() || u.calls.contains_key(&g) || !inlinable(f) {
                return None;
            }
            let loops = site_map[&g].iter().filter(|s| s.3).count();
            let size = crate::size::function(m, f, call_leb);
            (gain(m, g, f.args.len(), None, u, size, call_leb, loops)? > 0).then_some(())
        })
        .into_iter()
        .filter_map(|(g, ok)| ok.map(|_| g))
        .collect();
        let mut inlined = 0;
        for g in callee_first(&cands, &site_map) {
            let f = m.funcs.get(g).kind.unwrap_local();
            // The body may have grown since the candidates were chosen.
            let u = &uses[&g];
            let sites = &site_map[&g];
            let loops = sites.iter().filter(|s| s.3).count();
            let size = crate::size::function(m, f, call_leb);
            let snap = Snapshot::take(m, f);
            match gain(m, g, f.args.len(), Some(&snap), u, size, call_leb, loops) {
                Some(x) if x > 0 => {}
                _ => continue,
            }
            // Find the calls again: inlining other functions into the same
            // caller has moved them within their sequence (sequences stay).
            let mut groups: Vec<(FunctionId, InstrSeqId, bool, Vec<usize>)> = Vec::new();
            for &(caller, seq, _, in_loop) in sites {
                if groups.iter().any(|x| x.0 == caller && x.1 == seq) {
                    continue;
                }
                let at: Vec<usize> = m
                    .funcs
                    .get(caller)
                    .kind
                    .unwrap_local()
                    .block(seq)
                    .instrs
                    .iter()
                    .enumerate()
                    .filter(|(_, (i, _))| matches!(i, Instr::Call(c) if c.func == g))
                    .map(|(k, _)| k)
                    .collect();
                groups.push((caller, seq, in_loop, at));
            }
            if groups.iter().map(|x| x.3.len()).sum::<usize>() != sites.len() {
                continue; // should not happen; never delete a function still called
            }
            for (caller, seq, in_loop, at) in groups {
                for &k in at.iter().rev() {
                    inline_site(m, caller, seq, k, in_loop, &snap);
                    stats.inlined_sites += 1;
                }
            }
            m.funcs.delete(g);
            stats.inlined_functions += 1;
            inlined += 1;
        }
        if inlined == 0 {
            break;
        }
    }
    stats
}
