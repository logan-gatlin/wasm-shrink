//! Local variable passes.
//!
//! `coalesce`: liveness-based register allocation of locals (binaryen's
//! `coalesce-locals`). Locals whose live ranges never overlap share one slot, so
//! a function declares fewer locals and more of them fit in a one-byte LEB index.
//! It matters most after inlining, which gives every inlined body fresh locals.
//! Slots are numbered most-used first (binaryen's `reorder-locals`). The same
//! liveness shows which writes are never read: a dead `local.set` becomes
//! `drop` and a dead `local.tee` is removed.
//!
//! `redundant`: a `const; local.set $x` is removed when `$x` already holds
//! that constant on every path to it (the constant case of binaryen's `rse`).
//!
//! `stackify`: peepholes on local traffic (the cheap part of binaryen's
//! `simplify-locals` and `vacuum`, and the `local2stack` step of its
//! stack-IR writer):
//! - `local.set $x  <A>  local.get $x` → `local.tee $x  <A>` when `<A>` is
//!   balanced: the value waits on the stack under `<A>` (see `carry`);
//! - `local.set $x; local.get $x` where those are the only uses of `$x` → nothing;
//! - `local.set $x; local.get $x` otherwise → `local.tee $x`;
//! - `local.get $x; local.set $x` → nothing;
//! - a `local.set` of a never-read local → `drop`, a `local.tee` of one → nothing;
//! - `local.tee $x; local.set $x` and `local.tee $x; drop` → `local.set $x`;
//! - `local.get`/`const`/`ref.null`/`ref.func` followed by `drop` → nothing.

use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use walrus::ir::*;
use walrus::{LocalFunction, LocalId, Module, ValType};

#[derive(Default, Debug)]
pub struct CoalesceStats {
    pub functions: usize,
    pub skipped_eh: usize,
    pub dead_stores: usize,
    pub locals_before: usize,
    pub locals_after: usize,
}

#[derive(Default, Debug)]
pub struct StackifyStats {
    pub removed_pairs: usize,
    pub tees: usize,
    pub dead_stores: usize,
    pub dropped_pure: usize,
}

#[derive(Clone, Copy)]
enum Ev {
    Get(u32),
    /// A write; the second field is a local copied from by `local.get b; local.set a`,
    /// the third indexes `Cfg::sets`.
    Set(u32, Option<u32>, u32),
}

struct Block {
    evs: Vec<Ev>,
    succ: Vec<usize>,
}

struct Cfg {
    blocks: Vec<Block>,
    targets: HashMap<InstrSeqId, usize>,
    ids: HashMap<LocalId, u32>,
    locals: Vec<LocalId>,
    uses: Vec<u32>,
    has_eh: bool,
    /// Where each write is: (sequence, index).
    sets: Vec<(InstrSeqId, usize)>,
    /// The constant each write stores, for `const; local.set`.
    set_const: Vec<Option<Value>>,
    /// Catch targets of the enclosing `try_table`s; a call or throw inside one
    /// may continue at any of them.
    catches: Vec<Vec<usize>>,
}

impl Cfg {
    fn new_block(&mut self) -> usize {
        self.blocks.push(Block {
            evs: Vec::new(),
            succ: Vec::new(),
        });
        self.blocks.len() - 1
    }
    fn edge(&mut self, a: usize, b: usize) {
        self.blocks[a].succ.push(b);
    }
    fn id(&mut self, l: LocalId) -> u32 {
        let n = self.locals.len() as u32;
        let id = *self.ids.entry(l).or_insert(n);
        if id == n {
            self.locals.push(l);
            self.uses.push(0);
        }
        self.uses[id as usize] += 1;
        id
    }

    /// Append `seq` to the CFG starting in block `cur`; returns the block that is
    /// current at the end of the sequence.
    fn walk(&mut self, f: &LocalFunction, seq: InstrSeqId, mut cur: usize) -> usize {
        let instrs = &f.block(seq).instrs;
        for (k, (instr, _)) in instrs.iter().enumerate() {
            match instr {
                Instr::LocalGet(g) => {
                    let id = self.id(g.local);
                    self.blocks[cur].evs.push(Ev::Get(id));
                }
                Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local }) => {
                    let copy = match k.checked_sub(1).map(|j| &instrs[j].0) {
                        Some(Instr::LocalGet(g)) => Some(g.local),
                        _ => None,
                    };
                    let id = self.id(*local);
                    let copy = copy.and_then(|c| self.ids.get(&c).copied());
                    self.sets.push((seq, k));
                    self.set_const
                        .push(match k.checked_sub(1).map(|j| &instrs[j].0) {
                            Some(Instr::Const(c)) => Some(c.value),
                            _ => None,
                        });
                    let site = self.sets.len() as u32 - 1;
                    self.blocks[cur].evs.push(Ev::Set(id, copy, site));
                }
                Instr::Block(b) => {
                    let after = self.new_block();
                    self.targets.insert(b.seq, after);
                    let end = self.walk(f, b.seq, cur);
                    self.edge(end, after);
                    cur = after;
                }
                Instr::Loop(l) => {
                    let head = self.new_block();
                    self.edge(cur, head);
                    self.targets.insert(l.seq, head);
                    let end = self.walk(f, l.seq, head);
                    let after = self.new_block();
                    self.edge(end, after);
                    cur = after;
                }
                Instr::IfElse(i) => {
                    let after = self.new_block();
                    self.targets.insert(i.consequent, after);
                    self.targets.insert(i.alternative, after);
                    let (t, e) = (self.new_block(), self.new_block());
                    self.edge(cur, t);
                    self.edge(cur, e);
                    let te = self.walk(f, i.consequent, t);
                    let ee = self.walk(f, i.alternative, e);
                    self.edge(te, after);
                    self.edge(ee, after);
                    cur = after;
                }
                Instr::TryTable(t) => {
                    let mut here = Vec::new();
                    for c in &t.catches {
                        match c {
                            TryTableCatch::Catch { label, .. }
                            | TryTableCatch::CatchRef { label, .. }
                            | TryTableCatch::CatchAll { label }
                            | TryTableCatch::CatchAllRef { label } => {
                                here.push(self.targets[label])
                            }
                        }
                    }
                    let after = self.new_block();
                    self.targets.insert(t.seq, after);
                    self.catches.push(here);
                    let end = self.walk(f, t.seq, cur);
                    self.catches.pop();
                    self.edge(end, after);
                    cur = after;
                }
                Instr::Try(_) => {
                    // Legacy exceptions: `delegate`/`rethrow` use depths; not modelled.
                    self.has_eh = true;
                    return cur;
                }
                Instr::Call(_) | Instr::CallIndirect(_) | Instr::CallRef(_)
                    if !self.catches.is_empty() =>
                {
                    let all: Vec<usize> = self.catches.iter().flatten().copied().collect();
                    for t in all {
                        self.edge(cur, t);
                    }
                    let n = self.new_block();
                    self.edge(cur, n);
                    cur = n;
                }
                Instr::Br(b) => {
                    let t = self.targets[&b.block];
                    self.edge(cur, t);
                    cur = self.new_block();
                }
                Instr::BrIf(BrIf { block })
                | Instr::BrOnNull(BrOnNull { block })
                | Instr::BrOnNonNull(BrOnNonNull { block })
                | Instr::BrOnCast(BrOnCast { block, .. })
                | Instr::BrOnCastFail(BrOnCastFail { block, .. }) => {
                    let t = self.targets[block];
                    self.edge(cur, t);
                    let n = self.new_block();
                    self.edge(cur, n);
                    cur = n;
                }
                Instr::BrTable(b) => {
                    for s in b.blocks.iter().chain([&b.default]) {
                        let t = self.targets[s];
                        self.edge(cur, t);
                    }
                    cur = self.new_block();
                }
                Instr::Throw(_) | Instr::ThrowRef(_) => {
                    let all: Vec<usize> = self.catches.iter().flatten().copied().collect();
                    for t in all {
                        self.edge(cur, t);
                    }
                    cur = self.new_block();
                }
                Instr::Return(_)
                | Instr::Unreachable(_)
                | Instr::Rethrow(_)
                | Instr::ReturnCall(_)
                | Instr::ReturnCallIndirect(_)
                | Instr::ReturnCallRef(_) => {
                    cur = self.new_block();
                }
                _ => {}
            }
        }
        cur
    }
}

struct BitSet(Vec<u64>);
impl BitSet {
    fn new(n: usize) -> BitSet {
        BitSet(vec![0; n.div_ceil(64)])
    }
    fn has(&self, i: u32) -> bool {
        self.0[i as usize / 64] >> (i % 64) & 1 == 1
    }
    fn set(&mut self, i: u32) {
        self.0[i as usize / 64] |= 1 << (i % 64);
    }
    fn clear(&mut self, i: u32) {
        self.0[i as usize / 64] &= !(1 << (i % 64));
    }
    fn union(&mut self, o: &BitSet) -> bool {
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            let n = *a | *b;
            changed |= n != *a;
            *a = n;
        }
        changed
    }
    /// `self |= gen | (out & !kill)`; whether `self` grew.
    fn union_transfer(&mut self, gen_: &BitSet, out: &BitSet, kill: &BitSet) -> bool {
        let mut changed = false;
        for (((a, g), o), k) in self.0.iter_mut().zip(&gen_.0).zip(&out.0).zip(&kill.0) {
            let n = *a | *g | (*o & !*k);
            changed |= n != *a;
            *a = n;
        }
        changed
    }
    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(w, &bits)| {
            let mut bits = bits;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let b = bits.trailing_zeros();
                bits &= bits - 1;
                Some((w * 64) as u32 + b)
            })
        })
    }
}

/// The colouring of one function, ready to apply.
struct Assign {
    /// The function's locals, parameters first.
    locals: Vec<LocalId>,
    /// Colour of each entry of `locals`.
    color: Vec<u32>,
    /// Type of each colour.
    color_ty: Vec<ValType>,
    /// Non-parameter colours, in the order their fresh locals are allocated.
    alloc_order: Vec<usize>,
    nparams: usize,
    /// Writes whose value is never read: (sequence, index).
    dead: Vec<(InstrSeqId, usize)>,
}

enum Plan {
    Nothing,
    SkippedEh,
    Assign(Assign),
}

/// The function's control-flow graph and its entry block.
fn build_cfg(f: &LocalFunction) -> (Cfg, usize) {
    let mut cfg = Cfg {
        blocks: Vec::new(),
        targets: HashMap::new(),
        ids: HashMap::new(),
        locals: Vec::new(),
        uses: Vec::new(),
        has_eh: false,
        sets: Vec::new(),
        set_const: Vec::new(),
        catches: Vec::new(),
    };
    // Parameters take the first ids so they are easy to recognise.
    for &a in &f.args {
        cfg.id(a);
        cfg.uses[cfg.ids[&a] as usize] -= 1;
    }
    let entry = cfg.new_block();
    let body = f.entry_block();
    // The function body is a branch target meaning "return".
    let exit = cfg.new_block();
    cfg.targets.insert(body, exit);
    let end = cfg.walk(f, body, entry);
    cfg.edge(end, exit);
    (cfg, entry)
}

/// Liveness, interference and colouring: everything except allocating the new
/// locals, which must happen in a fixed order across functions.
fn plan(m: &Module, f: &LocalFunction) -> Plan {
    let (cfg, entry) = build_cfg(f);
    let nparams = f.args.len() as u32;
    if cfg.has_eh {
        return Plan::SkippedEh;
    }
    let n = cfg.locals.len();
    if n as u32 <= nparams {
        return Plan::Nothing;
    }
    // Backward liveness to a fixed point, with a worklist. Each block's events
    // collapse to live_in = gen ∪ (live_out ∖ kill), and a block is revisited
    // only when the live-in of one of its successors grew.
    let nb = cfg.blocks.len();
    let mut gen_kill: Vec<(BitSet, BitSet)> = Vec::with_capacity(nb);
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
    for (b, block) in cfg.blocks.iter().enumerate() {
        let (mut g, mut k) = (BitSet::new(n), BitSet::new(n));
        for ev in block.evs.iter().rev() {
            match *ev {
                Ev::Get(l) => g.set(l),
                Ev::Set(l, ..) => {
                    g.clear(l);
                    k.set(l);
                }
            }
        }
        gen_kill.push((g, k));
        for &s in &block.succ {
            preds[s].push(b);
        }
    }
    let mut live_in: Vec<BitSet> = (0..nb).map(|_| BitSet::new(n)).collect();
    let mut work: Vec<usize> = (0..nb).collect();
    let mut queued = vec![true; nb];
    let mut out = BitSet::new(n);
    while let Some(b) = work.pop() {
        queued[b] = false;
        out.0.fill(0);
        for &s in &cfg.blocks[b].succ {
            out.union(&live_in[s]);
        }
        let (g, k) = &gen_kill[b];
        if live_in[b].union_transfer(g, &out, k) {
            for &p in &preds[b] {
                if !queued[p] {
                    queued[p] = true;
                    work.push(p);
                }
            }
        }
    }
    // Interference: a write interferes with everything live after it, except
    // the local it copies from.
    let mut interf: Vec<BitSet> = (0..n).map(|_| BitSet::new(n)).collect();
    let mut dead: Vec<(InstrSeqId, usize)> = Vec::new();
    let add = |interf: &mut Vec<BitSet>, a: u32, b: u32| {
        if a != b {
            interf[a as usize].set(b);
            interf[b as usize].set(a);
        }
    };
    for b in 0..nb {
        let mut live = BitSet::new(n);
        for &s in &cfg.blocks[b].succ {
            live.union(&live_in[s]);
        }
        for ev in cfg.blocks[b].evs.iter().rev() {
            match *ev {
                Ev::Get(l) => live.set(l),
                Ev::Set(l, copy, site) => {
                    if !live.has(l) {
                        // Never read: the write is removed, so it cannot
                        // clobber anything.
                        dead.push(cfg.sets[site as usize]);
                        continue;
                    }
                    live.clear(l);
                    for o in live.iter() {
                        if Some(o) != copy {
                            add(&mut interf, l, o);
                        }
                    }
                }
            }
        }
    }
    // Entry: parameters hold arguments, every other local holds zero. A local
    // read before any write (live at entry) relies on that zero, so it must not
    // share a parameter's slot; parameters never share with each other.
    let at_entry: Vec<u32> = live_in[entry].iter().collect();
    for &l in &at_entry {
        for p in 0..nparams {
            add(&mut interf, l, p);
        }
        for &o in &at_entry {
            add(&mut interf, l, o);
        }
    }
    for p in 0..nparams {
        for q in 0..nparams {
            add(&mut interf, p, q);
        }
    }

    // Greedy colouring, most-used locals first; parameters keep their slots.
    let tys: Vec<ValType> = cfg.locals.iter().map(|&l| m.locals.get(l).ty()).collect();
    let mut color: Vec<u32> = vec![u32::MAX; n];
    let mut members: Vec<Vec<u32>> = Vec::new();
    let mut color_ty: Vec<ValType> = Vec::new();
    for p in 0..nparams {
        color[p as usize] = p;
        members.push(vec![p]);
        color_ty.push(tys[p as usize]);
    }
    let mut order: Vec<u32> = (nparams..n as u32).collect();
    order.sort_by_key(|&l| std::cmp::Reverse(cfg.uses[l as usize]));
    for l in order {
        let c = (0..members.len()).find(|&c| {
            color_ty[c] == tys[l as usize] && members[c].iter().all(|&o| !interf[l as usize].has(o))
        });
        let c = match c {
            Some(c) => c,
            None => {
                members.push(Vec::new());
                color_ty.push(tys[l as usize]);
                members.len() - 1
            }
        };
        color[l as usize] = c as u32;
        members[c].push(l);
    }
    // Fresh locals for the non-parameter colours, allocated most-used first so
    // walrus (which orders locals by id within a type) gives them low indices.
    let mut weight: Vec<(u32, usize)> = (nparams as usize..members.len())
        .map(|c| {
            (
                members[c]
                    .iter()
                    .map(|&l| cfg.uses[l as usize])
                    .sum::<u32>(),
                c,
            )
        })
        .collect();
    weight.sort_by_key(|&(w, c)| (std::cmp::Reverse(w), c));
    Plan::Assign(Assign {
        locals: cfg.locals,
        color,
        color_ty,
        alloc_order: weight.into_iter().map(|(_, c)| c).collect(),
        nparams: nparams as usize,
        dead,
    })
}

pub fn coalesce(m: &mut Module) -> CoalesceStats {
    let mut stats = CoalesceStats::default();
    let plans = crate::util::par_map(m, |_, f| plan(m, f));
    // Fresh locals, allocated in function order then most-used first, so
    // walrus (which orders locals by id within a type) gives the busiest low
    // indices.
    let mut renames: HashMap<walrus::FunctionId, HashMap<LocalId, LocalId>> = HashMap::new();
    let mut dead: HashMap<walrus::FunctionId, Vec<(InstrSeqId, usize)>> = HashMap::new();
    for (id, p) in plans {
        let mut a = match p {
            Plan::Nothing => continue,
            Plan::SkippedEh => {
                stats.skipped_eh += 1;
                continue;
            }
            Plan::Assign(a) => a,
        };
        let mut slot: Vec<LocalId> = a.locals[..a.nparams].to_vec();
        slot.resize(a.color_ty.len(), a.locals[0]);
        for &c in &a.alloc_order {
            slot[c] = m.locals.add(a.color_ty[c]);
        }
        stats.functions += 1;
        stats.dead_stores += a.dead.len();
        dead.insert(id, std::mem::take(&mut a.dead));
        stats.locals_before += a.locals.len() - a.nparams;
        stats.locals_after += a.color_ty.len() - a.nparams;
        let map = a
            .locals
            .iter()
            .zip(&a.color)
            .map(|(&l, &c)| (l, slot[c as usize]))
            .collect();
        renames.insert(id, map);
    }
    struct Rename<'a>(&'a HashMap<LocalId, LocalId>);
    impl VisitorMut for Rename<'_> {
        fn visit_local_id_mut(&mut self, l: &mut LocalId) {
            if let Some(n) = self.0.get(l) {
                *l = *n;
            }
        }
    }
    m.funcs.par_iter_local_mut().for_each(|(id, f)| {
        if let Some(sites) = dead.get(&id) {
            // A dead `local.set` still has to pop its operand; a dead
            // `local.tee` just goes. Later indices first, so earlier ones hold.
            let mut sites = sites.clone();
            sites.sort_by_key(|&(_, k)| std::cmp::Reverse(k));
            for (s, k) in sites {
                let instrs = &mut f.block_mut(s).instrs;
                match instrs[k].0 {
                    Instr::LocalSet(_) => instrs[k].0 = Drop {}.into(),
                    _ => {
                        instrs.remove(k);
                    }
                }
            }
        }
        if let Some(map) = renames.get(&id) {
            let entry = f.entry_block();
            dfs_pre_order_mut(&mut Rename(map), f, entry);
        }
    });
    stats
}

#[derive(Default, Debug)]
pub struct RedundantStats {
    pub removed: usize,
}

fn bits(v: Value) -> Option<u64> {
    Some(match v {
        Value::I32(x) => x as u32 as u64,
        Value::I64(x) => x as u64,
        Value::F32(x) => x.to_bits() as u64,
        Value::F64(x) => x.to_bits(),
        Value::V128(_) => return None,
    })
}

/// `const; local.set` (or `local.tee`) writes that store the constant the
/// local already holds on every path: (sequence, index of the write).
///
/// A forward must-analysis over facts "local `l` holds constant `c`". A
/// write of `c` to `l` makes the fact true, any other write to `l` makes it
/// false, and it is true at a join only if true on every incoming edge.
fn redundant_sets(f: &LocalFunction) -> Vec<(InstrSeqId, usize)> {
    let (cfg, entry) = build_cfg(f);
    if cfg.has_eh {
        return Vec::new();
    }
    let nparams = f.args.len() as u32;
    let constant = |site: u32| cfg.set_const[site as usize].and_then(bits);
    // Only facts that could make a write redundant are tracked: a constant
    // written to the local twice, or zero (a non-parameter local starts as
    // zero).
    let mut seen: HashMap<(u32, u64), u32> = HashMap::new();
    for b in &cfg.blocks {
        for ev in &b.evs {
            if let Ev::Set(l, _, site) = *ev
                && let Some(c) = constant(site)
            {
                *seen.entry((l, c)).or_default() += 1;
            }
        }
    }
    let mut facts: Vec<(u32, u64)> = seen
        .into_iter()
        .filter(|&((l, c), n)| n > 1 || (c == 0 && l >= nparams))
        .map(|(k, _)| k)
        .collect();
    if facts.is_empty() {
        return Vec::new();
    }
    // Sorted, so the facts about one local are a contiguous range.
    facts.sort_unstable();
    let of_local = |l: u32| {
        let lo = facts.partition_point(|&(x, _)| x < l);
        let hi = facts.partition_point(|&(x, _)| x <= l);
        lo as u32..hi as u32
    };
    let fact = |l: u32, c: u64| facts.binary_search(&(l, c)).ok().map(|i| i as u32);
    // What a write does: every fact about the local becomes false (the
    // range), then the one it establishes, if any, true.
    let write = |l: u32, site: u32| (of_local(l), constant(site).and_then(|c| fact(l, c)));
    let nb = cfg.blocks.len();
    let n = facts.len();
    let mut gen_: Vec<BitSet> = Vec::with_capacity(nb);
    let mut kill: Vec<BitSet> = Vec::with_capacity(nb);
    for b in &cfg.blocks {
        let (mut g, mut k) = (BitSet::new(n), BitSet::new(n));
        for ev in &b.evs {
            if let Ev::Set(l, _, site) = *ev {
                let (falsified, established) = write(l, site);
                for f in falsified {
                    g.clear(f);
                    k.set(f);
                }
                if let Some(f) = established {
                    g.set(f);
                }
            }
        }
        gen_.push(g);
        kill.push(k);
    }
    // Facts at the start of each block. Everything holds in a block not
    // reached yet; at the function's entry, only the initial zeros.
    let mut at_entry: Vec<BitSet> = (0..nb).map(|_| BitSet(vec![!0; n.div_ceil(64)])).collect();
    at_entry[entry] = BitSet::new(n);
    for (i, &(l, c)) in facts.iter().enumerate() {
        if c == 0 && l >= nparams {
            at_entry[entry].set(i as u32);
        }
    }
    let mut reached = vec![false; nb];
    reached[entry] = true;
    let mut work = vec![entry];
    let mut queued = vec![false; nb];
    queued[entry] = true;
    let mut out = BitSet::new(n);
    while let Some(b) = work.pop() {
        queued[b] = false;
        for (((o, i), g), k) in out
            .0
            .iter_mut()
            .zip(&at_entry[b].0)
            .zip(&gen_[b].0)
            .zip(&kill[b].0)
        {
            *o = *g | (*i & !*k);
        }
        for &s in &cfg.blocks[b].succ {
            let mut changed = !reached[s];
            reached[s] = true;
            for (t, o) in at_entry[s].0.iter_mut().zip(&out.0) {
                changed |= *t & *o != *t;
                *t &= *o;
            }
            if changed && !queued[s] {
                queued[s] = true;
                work.push(s);
            }
        }
    }
    let mut redundant = Vec::new();
    for (b, block) in cfg.blocks.iter().enumerate() {
        if !reached[b] {
            continue;
        }
        let state = &mut at_entry[b];
        for ev in &block.evs {
            if let Ev::Set(l, _, site) = *ev {
                let (falsified, established) = write(l, site);
                if established.is_some_and(|f| state.has(f)) {
                    redundant.push(cfg.sets[site as usize]);
                }
                for f in falsified {
                    state.clear(f);
                }
                if let Some(f) = established {
                    state.set(f);
                }
            }
        }
    }
    redundant
}

/// Remove writes of a constant to a local that already holds it (the part
/// of binaryen's `rse` that matters here). LLVM lowers a phi to a write in
/// every predecessor, and after inlining many of those repeat the write
/// before them; a local's first write of zero repeats its initial value.
pub fn redundant(m: &mut Module) -> RedundantStats {
    let removed = m
        .funcs
        .par_iter_local_mut()
        .map(|(_, f)| {
            let mut sites = redundant_sets(f);
            // Later indices first, so earlier ones stay valid.
            sites.sort_by_key(|&(_, k)| std::cmp::Reverse(k));
            for &(s, k) in &sites {
                let instrs = &mut f.block_mut(s).instrs;
                match instrs[k].0 {
                    // `const; local.set`: both go.
                    Instr::LocalSet(_) => drop(instrs.drain(k - 1..=k)),
                    // `const; local.tee`: the constant stays on the stack.
                    _ => drop(instrs.remove(k)),
                }
            }
            sites.len()
        })
        .sum();
    RedundantStats { removed }
}

fn all_seqs(f: &LocalFunction) -> Vec<InstrSeqId> {
    let mut out = Vec::new();
    let mut stack = vec![f.entry_block()];
    while let Some(s) = stack.pop() {
        out.push(s);
        for (i, _) in &f.block(s).instrs {
            match i {
                Instr::Block(b) => stack.push(b.seq),
                Instr::Loop(l) => stack.push(l.seq),
                Instr::IfElse(e) => stack.extend([e.consequent, e.alternative]),
                Instr::TryTable(t) => stack.push(t.seq),
                Instr::Try(t) => {
                    stack.push(t.seq);
                    for c in &t.catches {
                        if let LegacyCatch::Catch { handler, .. }
                        | LegacyCatch::CatchAll { handler } = c
                        {
                            stack.push(*handler);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn stackify_function(f: &mut LocalFunction, params: &[LocalId], stats: &mut StackifyStats) -> bool {
    let seqs = all_seqs(f);
    // (gets, sets+tees) per local.
    let mut count: HashMap<LocalId, (u32, u32)> = HashMap::new();
    for &s in &seqs {
        for (i, _) in &f.block(s).instrs {
            match i {
                Instr::LocalGet(g) => count.entry(g.local).or_default().0 += 1,
                Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local }) => {
                    count.entry(*local).or_default().1 += 1
                }
                _ => {}
            }
        }
    }
    let mut changed = false;
    for s in seqs {
        let instrs = &mut f.block_mut(s).instrs;
        let mut out: Vec<(Instr, InstrLocId)> = Vec::with_capacity(instrs.len());
        for (i, loc) in instrs.drain(..) {
            let prev = out.last().map(|p| &p.0);
            match (prev, &i) {
                (Some(Instr::LocalSet(a)), Instr::LocalGet(b)) if a.local == b.local => {
                    let c = count[&a.local];
                    if c == (1, 1) {
                        out.pop();
                        stats.removed_pairs += 1;
                    } else {
                        let l = a.local;
                        out.pop();
                        out.push((LocalTee { local: l }.into(), loc));
                        stats.tees += 1;
                    }
                    changed = true;
                    continue;
                }
                (Some(Instr::LocalGet(a)), Instr::LocalSet(b)) if a.local == b.local => {
                    out.pop();
                    stats.removed_pairs += 1;
                    changed = true;
                    continue;
                }
                (Some(Instr::LocalTee(a)), Instr::LocalSet(b)) if a.local == b.local => {
                    let l = a.local;
                    out.pop();
                    out.push((LocalSet { local: l }.into(), loc));
                    stats.tees += 1;
                    changed = true;
                    continue;
                }
                (Some(Instr::LocalTee(a)), Instr::Drop(_)) => {
                    let l = a.local;
                    out.pop();
                    out.push((LocalSet { local: l }.into(), loc));
                    stats.tees += 1;
                    changed = true;
                    continue;
                }
                (
                    Some(
                        Instr::LocalGet(_)
                        | Instr::Const(_)
                        | Instr::RefNull(_)
                        | Instr::RefFunc(_),
                    ),
                    Instr::Drop(_),
                ) => {
                    out.pop();
                    stats.dropped_pure += 1;
                    changed = true;
                    continue;
                }
                _ => {}
            }
            match &i {
                Instr::LocalSet(x) if count[&x.local].0 == 0 && !params.contains(&x.local) => {
                    out.push((Drop {}.into(), loc));
                    stats.dead_stores += 1;
                    changed = true;
                }
                Instr::LocalTee(x) if count[&x.local].0 == 0 => {
                    stats.dead_stores += 1;
                    changed = true;
                }
                _ => out.push((i, loc)),
            }
        }
        *instrs = out;
    }
    changed
}

/// `local.set $x  <A>  local.get $x` → `local.tee $x  <A>`, and just `<A>`
/// when those are the only uses of `$x`.
///
/// `<A>` must leave the stack as it found it and never pop below where it
/// started. The value then stays on the stack under `<A>` until the point
/// where the `local.get` would have pushed it. No instruction moves, so
/// nothing is evaluated in a different order; `<A>` only must not write `$x`,
/// so that the `local.get` did read this value.
///
/// Pairs found in one sweep are nested or apart, never overlapping: if a
/// second value were stacked on top of the first, whatever popped the first
/// would get the second.
fn carry(m: &Module, f: &mut LocalFunction, stats: &mut StackifyStats) -> bool {
    let seqs = all_seqs(f);
    let mut count: HashMap<LocalId, (u32, u32)> = HashMap::new();
    let loops = crate::stack::loops(f);
    for &s in &seqs {
        for (i, _) in &f.block(s).instrs {
            match i {
                Instr::LocalGet(g) => count.entry(g.local).or_default().0 += 1,
                Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local }) => {
                    count.entry(*local).or_default().1 += 1
                }
                _ => {}
            }
        }
    }
    // Locals written anywhere inside each sequence. `all_seqs` lists parents
    // before children, so the reverse order sees children first.
    let mut writes: HashMap<InstrSeqId, HashSet<LocalId>> = HashMap::new();
    for &s in seqs.iter().rev() {
        let mut w = HashSet::new();
        for (i, _) in &f.block(s).instrs {
            match i {
                Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local })
                    if count[local].1 > 1 =>
                {
                    w.insert(*local);
                }
                _ => {
                    for c in crate::passes::branches::children(i) {
                        w.extend(writes[&c].iter().copied());
                    }
                }
            }
        }
        writes.insert(s, w);
    }
    let mut changed = false;
    for s in seqs {
        let instrs = &f.block(s).instrs;
        let n = instrs.len();
        // 0 = keep, 1 = remove, 2 = turn the set into a tee.
        let mut edit = vec![0u8; n];
        for i in 0..n {
            let Instr::LocalSet(LocalSet { local: x }) = instrs[i].0 else {
                continue;
            };
            let mut h = 0usize;
            for j in i + 1..n {
                let ins = &instrs[j].0;
                // A removed `get` belongs to a pair that straddles this set.
                if edit[j] == 1 {
                    break;
                }
                match ins {
                    Instr::LocalGet(g) if g.local == x && h == 0 => {
                        if j == i + 1 {
                            break; // adjacent: the peephole below handles it
                        }
                        edit[j] = 1;
                        if count[&x] == (1, 1) {
                            edit[i] = 1;
                            stats.removed_pairs += 1;
                        } else {
                            edit[i] = 2;
                            stats.tees += 1;
                        }
                        break;
                    }
                    Instr::LocalSet(LocalSet { local }) | Instr::LocalTee(LocalTee { local })
                        if *local == x =>
                    {
                        break;
                    }
                    _ => {}
                }
                if crate::passes::branches::children(ins)
                    .iter()
                    .any(|c| writes[c].contains(&x))
                {
                    break;
                }
                let Some((pop, push)) = crate::stack::effect_with_br_if(m, f, &loops, ins) else {
                    break;
                };
                if h < pop {
                    break;
                }
                h = h - pop + push;
            }
        }
        if edit.iter().all(|&e| e == 0) {
            continue;
        }
        changed = true;
        let old = std::mem::take(&mut f.block_mut(s).instrs);
        f.block_mut(s).instrs = old
            .into_iter()
            .zip(edit)
            .filter_map(|((i, loc), e)| match (e, &i) {
                (1, _) => None,
                (2, Instr::LocalSet(x)) => Some((LocalTee { local: x.local }.into(), loc)),
                _ => Some((i, loc)),
            })
            .collect();
    }
    changed
}

crate::sum_stats!(StackifyStats: removed_pairs, tees, dead_stores, dropped_pure);

pub fn stackify(m: &mut Module) -> StackifyStats {
    let mut stats = StackifyStats::default();
    for (_, s) in crate::util::par_take(m, |m, _, f| {
        let mut stats = StackifyStats::default();
        let params = f.args.clone();
        for _ in 0..8 {
            let a = carry(m, f, &mut stats);
            let b = stackify_function(f, &params, &mut stats);
            if !a && !b {
                break;
            }
        }
        stats
    }) {
        stats += s;
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::{coalesce, redundant, stackify};
    use crate::test_util::{flat, run};

    const PRELUDE: &str = "(memory 1) (func $g (result i32) i32.const 7)";

    fn check(pass: fn(&mut walrus::Module), body: &str, want: &str) {
        let wat = format!("(module {PRELUDE} (func $f (param i32) (result i32) {body}))");
        assert_eq!(run(&wat, pass), flat(want));
    }

    fn stackify_(m: &mut walrus::Module) {
        stackify(m);
    }
    fn coalesce_(m: &mut walrus::Module) {
        coalesce(m);
    }
    fn redundant_(m: &mut walrus::Module) {
        redundant(m);
    }

    #[test]
    fn carries_a_value_over_a_statement() {
        // The call's result waits on the stack while the store runs.
        check(
            stackify_,
            "(local i32)
             call $g  local.set 1
             local.get 0  i32.const 5  i32.store
             local.get 1",
            "call $g  local.get 0  i32.const 5  i32.store",
        );
    }

    #[test]
    fn carries_with_a_tee_when_the_local_is_read_again() {
        check(
            stackify_,
            "(local i32)
             call $g  local.set 1
             local.get 0  i32.const 5  i32.store
             local.get 1  local.get 1  i32.add",
            "(local i32)
             call $g  local.tee 1
             local.get 0  i32.const 5  i32.store
             local.get 1  i32.add",
        );
    }

    #[test]
    fn carries_over_a_block_and_a_br_if() {
        check(
            stackify_,
            "(local i32)
             call $g  local.set 1
             block  local.get 0  br_if 0  local.get 0  i32.const 5  i32.store  end
             local.get 1",
            "call $g
             block  local.get 0  br_if 0  local.get 0  i32.const 5  i32.store  end",
        );
    }

    #[test]
    fn does_not_carry_under_an_operand() {
        // `local.get 0` is pushed first, so the call's result cannot be
        // under it without reordering the two.
        let body = "(local i32)
             call $g  local.set 1
             local.get 0  local.get 1  i32.add";
        check(stackify_, body, body);
    }

    #[test]
    fn does_not_carry_past_a_write_of_the_local() {
        let body = "(local i32)
             call $g  local.set 1
             block  local.get 0  if  i32.const 3  local.set 1  end  end
             local.get 1  local.get 1  i32.add";
        check(stackify_, body, body);
    }

    #[test]
    fn carried_values_nest() {
        // `1` is carried over the second call; `2` is then an adjacent pair.
        check(
            stackify_,
            "(local i32 i32)
             call $g  local.set 1
             call $g  local.set 2
             local.get 1  local.get 2  i32.add",
            "call $g  call $g  i32.add",
        );
    }

    #[test]
    fn carrying_keeps_operand_order() {
        // `2` is carried to where it is read. `1` is read on top of it, so
        // it cannot wait underneath.
        check(
            stackify_,
            "(local i32 i32)
             call $g  local.set 1
             call $g  local.set 2
             local.get 0  i32.const 5  i32.store
             local.get 2
             local.get 0  i32.const 6  i32.store
             local.get 1
             i32.sub",
            "(local i32)
             call $g  local.set 1
             call $g
             local.get 0  i32.const 5  i32.store
             local.get 0  i32.const 6  i32.store
             local.get 1
             i32.sub",
        );
    }

    #[test]
    fn removes_a_write_that_is_never_read_again() {
        // The second write to `1` is dead; the first is read. With it gone
        // `1` and `2` never hold different live values and share a slot.
        check(
            coalesce_,
            "(local i32 i32)
             call $g  local.set 1
             local.get 1  local.set 2
             call $g  local.set 1
             local.get 2  local.get 0  i32.add",
            "(local i32)
             call $g  local.set 1
             local.get 1  local.set 1
             call $g  drop
             local.get 1  local.get 0  i32.add",
        );
    }

    #[test]
    fn removes_a_dead_tee() {
        check(
            coalesce_,
            "(local i32 i32)
             call $g  local.set 2
             call $g  local.tee 1  local.get 2  i32.add  local.get 0  i32.add",
            "(local i32)
             call $g  local.set 1
             call $g  local.get 1  i32.add  local.get 0  i32.add",
        );
    }

    #[test]
    fn a_local_can_take_over_a_dead_parameter() {
        // The parameter is never read, so the only local uses its slot.
        check(
            coalesce_,
            "(local i32)  call $g  local.set 1  call $g  local.get 1  i32.add",
            "call $g  local.set 0  call $g  local.get 0  i32.add",
        );
    }

    #[test]
    fn keeps_a_write_read_on_another_path() {
        // `2` is read at the top of the next iteration.
        let body = "(local i32 i32)
             call $g  local.set 1
             loop
               local.get 2  local.get 1  i32.add  local.set 1
               call $g  local.set 2
               local.get 0  br_if 0
             end
             local.get 1";
        check(coalesce_, body, body);
    }

    #[test]
    fn removes_a_write_of_the_initial_zero() {
        check(
            redundant_,
            "(local i32)  i32.const 0  local.set 1  local.get 1",
            "(local i32)  local.get 1",
        );
    }

    #[test]
    fn keeps_a_zero_written_to_a_parameter() {
        let body = "i32.const 0  local.set 0  local.get 0";
        check(redundant_, body, body);
    }

    #[test]
    fn removes_a_constant_written_again_on_every_path() {
        check(
            redundant_,
            "(local i32)
             i32.const 1  local.set 1
             local.get 0  if  i32.const 1  local.set 1  call $g  drop  end
             i32.const 1  local.tee 1",
            "(local i32)
             i32.const 1  local.set 1
             local.get 0  if  call $g  drop  end
             i32.const 1",
        );
    }

    #[test]
    fn keeps_a_constant_that_differs_on_one_path() {
        let body = "(local i32)
             local.get 0  if  i32.const 2  local.set 1  end
             i32.const 0  local.set 1
             local.get 1";
        check(redundant_, body, body);
    }

    #[test]
    fn keeps_a_constant_a_loop_may_have_changed() {
        // The first write of zero repeats the initial value; the one in the
        // loop does not, because the back edge arrives with 5.
        check(
            redundant_,
            "(local i32)
             i32.const 0  local.set 1
             loop
               i32.const 0  local.set 1
               i32.const 5  local.set 1
               local.get 0  br_if 0
             end
             local.get 1",
            "(local i32)
             loop
               i32.const 0  local.set 1
               i32.const 5  local.set 1
               local.get 0  br_if 0
             end
             local.get 1",
        );
    }

    #[test]
    fn keeps_a_constant_after_an_unknown_write() {
        let body = "(local i32)
             i32.const 4  local.set 1
             call $g  local.set 1
             i32.const 4  local.set 1
             local.get 1";
        check(redundant_, body, body);
    }
}
