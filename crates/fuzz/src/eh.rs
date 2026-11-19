use arbitrary::{Result, Unstructured};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use wasm_shrink::Pass;

pub struct EhCase {
    pub wasm: Vec<u8>,
    pub passes: Vec<Pass>,
}

impl EhCase {
    pub fn generate(data: &[u8]) -> Result<EhCase> {
        let mut u = Unstructured::new(data);
        let mut passes: Vec<Pass> = if u.ratio(1, 3)? {
            Pass::DEFAULT.to_vec()
        } else {
            (0..u.int_in_range(0..=6)?)
                .map(|_| u.choose(Pass::ALL).copied())
                .collect::<Result<_>>()?
        };
        if u.ratio(3, 4)? {
            let at = u.int_in_range(0..=passes.len())?;
            passes.insert(at, Pass::TranslateToExnref);
        }
        let wat = program(&mut u)?;
        let wasm = wat::parse_str(&wat).unwrap_or_else(|e| panic!("{e}\n{wat}"));
        Ok(EhCase { wasm, passes })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Func,
    Block,
    Loop,
    If,
    Try,
    Catch,
}

struct Gen<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    funcs: usize,
    cur: usize,
    labels: Vec<(Kind, bool)>,
    counters: usize,
    fuel: usize,
    out: String,
}

const TAGS: [&str; 3] = ["$e0", "$e1", "$e2"];

fn program(u: &mut Unstructured) -> Result<String> {
    let funcs = u.int_in_range(1..=6)?;
    let mut wat = String::from(
        "(module
  (import \"env\" \"log\" (func $log (param i32)))
  (tag $e0 (export \"e0\") (param i32))
  (tag $e1 (export \"e1\"))
  (tag $e2 (export \"e2\") (param i32 i32))
  (global $g (export \"g\") (mut i32) (i32.const 0))
",
    );
    for cur in 0..funcs {
        let mut g = Gen {
            u,
            funcs,
            cur,
            labels: vec![(Kind::Func, true)],
            counters: 0,
            fuel: 40,
            out: String::new(),
        };
        g.stmts(0)?;
        g.expr(2)?;
        let export = if cur == 0 || g.u.arbitrary()? {
            format!(" (export \"f{cur}\")")
        } else {
            String::new()
        };
        let _ = write!(
            wat,
            "  (func $f{cur}{export} (param $a i32) (result i32) (local $x0 i32) (local $x1 i32)"
        );
        for c in 0..g.counters {
            let _ = write!(wat, " (local $c{c} i32)");
        }
        let _ = writeln!(wat, "\n{}\n  )", g.out);
    }
    wat.push(')');
    Ok(wat)
}

impl Gen<'_, '_> {
    fn w(&mut self, s: &str) {
        self.out.push(' ');
        self.out.push_str(s);
    }

    fn depth_of(&self, index: usize) -> usize {
        self.labels.len() - 1 - index
    }

    fn local(&mut self) -> Result<&'static str> {
        Ok(if self.u.arbitrary()? { "$x0" } else { "$x1" })
    }

    fn expr(&mut self, d: usize) -> Result<()> {
        if d == 0 || self.u.ratio(1, 2)? {
            match self.u.int_in_range(0..=4)? {
                0 => self.w("local.get $a"),
                1 => self.w("local.get $x0"),
                2 => self.w("local.get $x1"),
                3 => self.w("global.get $g"),
                _ => {
                    let k: i8 = self.u.arbitrary()?;
                    self.w(&format!("i32.const {k}"));
                }
            }
            return Ok(());
        }
        if self.cur + 1 < self.funcs && self.u.ratio(1, 4)? {
            let j = self.u.int_in_range(self.cur + 1..=self.funcs - 1)?;
            self.expr(d - 1)?;
            self.w(&format!("call $f{j}"));
            return Ok(());
        }
        self.expr(d - 1)?;
        self.expr(d - 1)?;
        let op = *self
            .u
            .choose(&["i32.add", "i32.sub", "i32.xor", "i32.mul", "i32.and"])?;
        self.w(op);
        Ok(())
    }

    fn cond(&mut self) -> Result<()> {
        if self.u.arbitrary()? {
            self.w("local.get $a");
            let k = self.u.int_in_range(-2..=40)?;
            self.w(&format!("i32.const {k} i32.lt_s"));
        } else {
            self.expr(2)?;
            let k = self.u.int_in_range(1..=7)?;
            self.w(&format!("i32.const {k} i32.and"));
        }
        Ok(())
    }

    fn block_type(result: bool) -> &'static str {
        if result { " (result i32)" } else { "" }
    }

    fn body(&mut self, kind: Kind, result: bool, depth: usize) -> Result<()> {
        self.labels.push((kind, result));
        self.stmts(depth + 1)?;
        if result {
            self.expr(2)?;
        }
        self.labels.pop();
        Ok(())
    }

    fn stmts(&mut self, depth: usize) -> Result<()> {
        let n = self.u.int_in_range(0..=if depth > 4 { 1 } else { 4 })?;
        for _ in 0..n {
            if self.fuel == 0 {
                break;
            }
            self.fuel -= 1;
            self.stmt(depth)?;
        }
        Ok(())
    }

    fn stmt(&mut self, depth: usize) -> Result<()> {
        let nested = depth < 5;
        match self.u.int_in_range(0..=15)? {
            0 => {
                self.expr(2)?;
                self.w("call $log");
            }
            1 => {
                self.expr(2)?;
                let x = self.local()?;
                self.w(&format!("local.set {x}"));
            }
            2 => {
                self.w("global.get $g i32.const 31 i32.mul");
                self.expr(2)?;
                self.w("i32.add global.set $g");
            }
            3 if nested => {
                let result = self.u.arbitrary()?;
                self.cond()?;
                self.w(&format!("if{}", Self::block_type(result)));
                self.body(Kind::If, result, depth)?;
                self.w("else");
                self.body(Kind::If, result, depth)?;
                self.w("end");
                if result {
                    let x = self.local()?;
                    self.w(&format!("local.set {x}"));
                }
            }
            4 if nested => {
                let result = self.u.arbitrary()?;
                self.w(&format!("block{}", Self::block_type(result)));
                self.body(Kind::Block, result, depth)?;
                self.w("end");
                if result {
                    let x = self.local()?;
                    self.w(&format!("local.set {x}"));
                }
            }
            5 if nested => {
                let c = self.counters;
                self.counters += 1;
                let n = self.u.int_in_range(1..=3)?;
                self.w(&format!("i32.const {n} local.set $c{c} loop"));
                self.body(Kind::Loop, false, depth)?;
                self.w(&format!(
                    "local.get $c{c} i32.const 1 i32.sub local.tee $c{c} br_if 0 end"
                ));
            }
            6 | 7 => {
                let targets: Vec<usize> = (0..self.labels.len())
                    .filter(|&i| self.labels[i].0 != Kind::Loop)
                    .collect();
                let t = *self.u.choose(&targets)?;
                let result = self.labels[t].1;
                if self.u.arbitrary()? {
                    self.cond()?;
                    self.w("if");
                    self.labels.push((Kind::If, false));
                    if result {
                        self.expr(2)?;
                    }
                    let d = self.depth_of(t);
                    self.w(&format!("br {d}"));
                    self.labels.pop();
                    self.w("end");
                } else {
                    if result {
                        self.expr(2)?;
                    }
                    self.cond()?;
                    let d = self.depth_of(t);
                    self.w(&format!("br_if {d}"));
                    if result {
                        self.w("drop");
                    }
                }
            }
            8 | 9 => {
                self.cond()?;
                self.w("if");
                let tag = self.u.choose_index(TAGS.len())?;
                for _ in 0..[1, 0, 2][tag] {
                    self.expr(2)?;
                }
                self.w(&format!("throw {} end", TAGS[tag]));
            }
            10 if self.cur + 1 < self.funcs => {
                let j = self.u.int_in_range(self.cur + 1..=self.funcs - 1)?;
                self.expr(2)?;
                let x = self.local()?;
                self.w(&format!("call $f{j} local.set {x}"));
            }
            11 | 12 if nested => {
                let result = self.u.arbitrary()?;
                self.w(&format!("try{}", Self::block_type(result)));
                self.body(Kind::Try, result, depth)?;
                for (tag, &name) in TAGS.iter().enumerate() {
                    if self.u.ratio(1, 2)? {
                        self.w(&format!("catch {name}"));
                        for _ in 0..[1, 0, 2][tag] {
                            let x = self.local()?;
                            self.w(&format!("local.set {x}"));
                        }
                        self.body(Kind::Catch, result, depth)?;
                    }
                }
                if self.u.ratio(1, 2)? {
                    self.w("catch_all");
                    self.body(Kind::Catch, result, depth)?;
                }
                self.w("end");
                if result {
                    let x = self.local()?;
                    self.w(&format!("local.set {x}"));
                }
            }
            13 if nested => {
                let result = self.u.arbitrary()?;
                self.w(&format!("try{}", Self::block_type(result)));
                self.body(Kind::Try, result, depth)?;
                let t = self.u.choose_index(self.labels.len())?;
                let d = self.depth_of(t);
                self.w(&format!("delegate {d}"));
                if result {
                    let x = self.local()?;
                    self.w(&format!("local.set {x}"));
                }
            }
            14 => {
                let catches: Vec<usize> = (0..self.labels.len())
                    .filter(|&i| self.labels[i].0 == Kind::Catch)
                    .collect();
                let Ok(&t) = self.u.choose(&catches) else {
                    return Ok(());
                };
                self.cond()?;
                self.w("if");
                self.labels.push((Kind::If, false));
                let d = self.depth_of(t);
                self.w(&format!("rethrow {d}"));
                self.labels.pop();
                self.w("end");
            }
            15 => {
                self.cond()?;
                self.w("if");
                self.expr(2)?;
                self.w("return end");
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn legacy_ops(wasm: &[u8]) -> usize {
    use wasmparser::{Operator, Payload};
    let mut n = 0;
    for p in wasmparser::Parser::new(0).parse_all(wasm) {
        let Ok(Payload::CodeSectionEntry(body)) = p else {
            continue;
        };
        let Ok(ops) = body.get_operators_reader() else {
            continue;
        };
        for op in ops.into_iter().flatten() {
            if matches!(
                op,
                Operator::Try { .. }
                    | Operator::Catch { .. }
                    | Operator::CatchAll
                    | Operator::Delegate { .. }
                    | Operator::Rethrow { .. }
            ) {
                n += 1;
            }
        }
    }
    n
}

pub fn check_static(case: &EhCase) -> std::result::Result<Vec<u8>, String> {
    let out = crate::shrink(&case.wasm, &case.passes, true, false)
        .ok_or("walrus can't parse the input")?;
    crate::validate(&out.wasm).map_err(|e| format!("output does not validate: {e}"))?;
    let again = crate::shrink(&case.wasm, &case.passes, true, false).unwrap();
    if again.wasm != out.wasm {
        return Err("output is not deterministic".into());
    }
    let named = crate::shrink(&case.wasm, &case.passes, true, true).unwrap();
    if crate::sections_without_names(&named.wasm) != crate::sections_without_names(&out.wasm) {
        return Err("names change the output".into());
    }
    if case.passes.contains(&Pass::TranslateToExnref) {
        let left = legacy_ops(&out.wasm);
        if left > 0 {
            return Err(format!(
                "{left} legacy exception instructions left after translate-to-exnref"
            ));
        }
    }
    Ok(out.wasm)
}

pub const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/eh.mjs");

pub fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Worker {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    dir: PathBuf,
}

static WORKER: Mutex<Option<Worker>> = Mutex::new(None);

fn start() -> Worker {
    let mut child = Command::new("node")
        .arg(SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node runs");
    let dir = std::env::temp_dir().join(format!("wasm-shrink-eh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Worker {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        _child: child,
        dir,
    }
}

/// Run `input` and `output` under node and compare what every export does.
pub fn compare(input: &[u8], output: &[u8]) -> std::result::Result<(), String> {
    let mut guard = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    let w = guard.get_or_insert_with(start);
    let (a, b) = (w.dir.join("in.wasm"), w.dir.join("out.wasm"));
    std::fs::write(&a, input).unwrap();
    std::fs::write(&b, output).unwrap();
    let reply = (|| -> std::io::Result<std::result::Result<(), String>> {
        writeln!(w.stdin, "{}\t{}", a.display(), b.display())?;
        w.stdin.flush()?;
        let mut line = String::new();
        w.stdout.read_line(&mut line)?;
        match line.trim_end() {
            "ok" => Ok(Ok(())),
            "bad" => {
                let mut detail = String::new();
                loop {
                    line.clear();
                    if w.stdout.read_line(&mut line)? == 0 || line.trim_end() == "end" {
                        break;
                    }
                    detail.push_str(&line);
                }
                Ok(Err(detail))
            }
            other => Ok(Err(format!("node worker said {other:?}"))),
        }
    })();
    match reply {
        Ok(r) => r,
        Err(e) => {
            *guard = None;
            Err(format!("node worker failed: {e}"))
        }
    }
}

pub fn check_case(case: &EhCase) -> std::result::Result<(), String> {
    let out = check_static(case)?;
    compare(&case.wasm, &out)
}

pub fn check(data: &[u8]) {
    let Ok(case) = EhCase::generate(data) else {
        return;
    };
    if let Err(e) = check_case(&case) {
        panic!("{}\n{e}", passes_flag(&case.passes));
    }
}

pub fn passes_flag(passes: &[Pass]) -> String {
    let names: Vec<&str> = passes.iter().map(|p| p.name()).collect();
    format!(
        "--passes {}",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(",")
        }
    )
}
