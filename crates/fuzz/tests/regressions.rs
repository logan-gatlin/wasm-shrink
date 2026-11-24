//! Modules the optimizer once got wrong, each through the fuzzer's own check.

use wasm_shrink::Pass;
use wasm_shrink_fuzz::{Case, Checked, check_case};

/// Name, passes (empty for the default pipeline) and module.
const CASES: &[(&str, &[Pass], &str)] = &[
    (
        "a br_if out of an `if` arm still runs the code after the `if`",
        &[Pass::Branches],
        r#"(func (export "f") (param i32 i32) (result i32) (local $x i32)
             block $out
               local.get 0
               if $arm
                 local.get 1  br_if $arm
                 br $out
               end
               i32.const 7  local.set $x
             end
             local.get $x)"#,
    ),
    (
        "tails that differ in a NaN payload stay apart",
        &[Pass::FoldTails],
        r#"(func (export "f") (param i32) (result i64)
             local.get 0
             if
               f64.const nan:0x8  i64.reinterpret_f64  i64.const 17  i64.xor  return
             else
               f64.const nan:0x9  i64.reinterpret_f64  i64.const 17  i64.xor  return
             end
             unreachable)"#,
    ),
    (
        "a zero segment past the end of memory still fails instantiation",
        &[Pass::MemoryPacking],
        r#"(memory (export "m") 1) (data (i32.const 65535) "\00\00")"#,
    ),
    (
        "an element segment past the end of an unused table still fails instantiation",
        &[],
        r#"(table 1 funcref) (elem (i32.const 2) func) (func (export "f"))"#,
    ),
    (
        "custom sections survive",
        &[Pass::Dedupe],
        r#"(@custom "metadata" "7") (func (export "f"))"#,
    ),
    (
        "a table initializer follows a merged function",
        &[Pass::Dedupe],
        r#"(func (export "kept") (result i32) i32.const 7)
           (func $f (result i32) i32.const 7)
           (table (export "t") 1 funcref (ref.func $f))
           (func (export "call") (result i32) call $f)"#,
    ),
    (
        "sizes of the smallest i64",
        &[Pass::FoldTails, Pass::MergeSimilar],
        r#"(func (export "f") (result i64)
             i64.const -9223372036854775808  i64.const 7  i64.xor  return)
           (func (export "g") (result i64)
             i64.const -9223372036854775807  i64.const 7  i64.xor  return)"#,
    ),
    (
        "a thunk is not its target when their types are in different rec groups",
        &[Pass::Thunks],
        r#"(rec (type $a (func (result i32))) (type (struct)))
           (rec (type $b (func (result i32))) (type (struct (field i32))))
           (table 1 funcref)
           (elem (i32.const 0) func $thunk)
           (func $target (type $a) i32.const 7)
           (func $thunk (type $b) call $target)
           (func (export "f") (result i32) i32.const 0  call_indirect (type $b))"#,
    ),
    (
        "a shared tail does not read a non-nullable local written inside the body",
        &[Pass::FoldTails],
        r#"(type $s (struct))
           (global $g (export "g") (mut i32) (i32.const 0))
           (func $use (param (ref $s)) global.get $g  i32.const 1  i32.add  global.set $g)
           (func (export "f") (param i32) (local $r (ref $s))
             struct.new $s  local.set $r
             local.get 0  i32.eqz
             if  local.get $r  call $use  local.get $r  call $use  unreachable  end
             local.get 0  i32.const 1  i32.eq
             if  local.get $r  call $use  local.get $r  call $use  unreachable  end
             local.get $r  call $use  local.get $r  call $use  unreachable)"#,
    ),
    (
        "a write to a non-nullable local stays while anything reads the local",
        &[Pass::CoalesceLocals],
        r#"(type $s (struct))
           (global $g (export "g") (mut i32) (i32.const 0))
           (func (export "f") (local $r (ref $s))
             struct.new $s  local.set $r
             block $b
               block  br $b  end
               local.get $r  drop
               i32.const 1  global.set $g
             end)"#,
    ),
];

#[test]
fn regressions() {
    let mut failures = Vec::new();
    for &(name, passes, module) in CASES {
        let passes = if passes.is_empty() {
            Pass::DEFAULT
        } else {
            passes
        };
        let case = Case {
            wasm: wat::parse_str(format!("(module {module})")).unwrap(),
            passes: passes.to_vec(),
            reorder: true,
            args: vec![],
        };
        match std::panic::catch_unwind(|| check_case(&case)) {
            Ok(Ok(Checked::Executed)) => {}
            Ok(Ok(other)) => failures.push(format!("{name}: {other:?}")),
            Ok(Err(e)) => failures.push(format!("{name}: {e}")),
            Err(_) => failures.push(format!("{name}: panicked")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Large shapes that used to take minutes, overflow the stack, or come out
/// with more locals or parameters than a function may have.
#[test]
fn scale() {
    let log = r#"(import "env" "log_i32" (func $log (param i32)))"#;
    let repeat = |n: usize, f: &dyn Fn(usize) -> String| (0..n).map(f).collect::<String>();
    let nested = |depth: usize, body: &str| {
        format!("{}{body}{}", "block ".repeat(depth), " end".repeat(depth))
    };
    let cases: Vec<(&str, &[Pass], String)> = vec![
        (
            "a chain of thunks",
            &[Pass::Thunks],
            format!(
                r#"(func (export "f") call 1) {} (func)"#,
                repeat(19_999, &|i| format!("(func call {})", i + 2)),
            ),
        ),
        (
            "many locals",
            &[Pass::CoalesceLocals],
            format!(
                r#"(func (export "f") (local {}) {})"#,
                "i32 ".repeat(50_000),
                repeat(50_000, &|i| format!("i32.const 1 local.set {i} ")),
            ),
        ),
        (
            "more constants to hoist than a function may have locals",
            &[Pass::ConstHoist],
            format!(
                r#"(global $g (mut i32) (i32.const 0)) (func (export "f") {})"#,
                repeat(50_001, &|i| format!(
                    "i32.const {} global.set $g ",
                    1 << 20 | i
                )
                .repeat(4)),
            ),
        ),
        (
            "more sums to share than a function may have locals",
            &[Pass::AddrCse],
            format!(
                r#"(global $g (mut i32) (i32.const 0)) (func (export "f") (param i32) block {} end)"#,
                repeat(50_001, &|i| {
                    format!("local.get 0 i32.const {i} i32.add global.set $g ").repeat(4)
                }),
            ),
        ),
        (
            "more call sites to inline than the caller may have locals",
            &[Pass::Inline],
            format!(
                r#"{} (func $nop (param i32)) (func (export "f") (param i32) {})"#,
                repeat(130, &|i| format!(
                    r#"(func (export "d{i}") (result i32) i32.const {i})"#
                )),
                "local.get 0 i32.const 1 i32.add call $nop ".repeat(50_001),
            ),
        ),
        (
            "more constants to pass than a function may have parameters",
            &[Pass::MergeSimilar],
            format!(
                "{log} {}",
                repeat(2, &|f| format!(
                    r#"(func (export "f{f}") (param {}) {} {})"#,
                    "i32 ".repeat(995),
                    repeat(10, &|k| format!(
                        "local.get 0 i32.const {} i32.add call $log ",
                        100_000 * (f + 1) + k
                    )),
                    "local.get 1 call $log ".repeat(8_000),
                )),
            ),
        ),
        (
            "deeply nested blocks around locals",
            &[Pass::CoalesceLocals, Pass::RedundantSets],
            format!(
                r#"(func (export "f") (param i32) (result i32) (local i32) {} local.get 1)"#,
                nested(10_000, "local.get 0 local.set 1"),
            ),
        ),
        (
            "a deeply nested function with one caller",
            &[Pass::Inline],
            format!(
                r#"{log} (func $deep (param i32) {}) (func (export "f") (param i32) local.get 0 call $deep)"#,
                nested(10_000, "local.get 0 call $log"),
            ),
        ),
        (
            "deeply nested functions that differ in a constant",
            &[Pass::MergeSimilar],
            format!(
                "{log} {}",
                repeat(2, &|f| format!(
                    r#"(func (export "f{f}") (param i32) {})"#,
                    nested(
                        10_000,
                        &format!("local.get 0 i32.const {} i32.add call $log", 1000 + f)
                    ),
                )),
            ),
        ),
        (
            "nesting as deep as the recursive walkers still follow",
            &[
                Pass::Inline,
                Pass::MergeSimilar,
                Pass::RedundantSets,
                Pass::CoalesceLocals,
            ],
            format!(
                r#"{log} {} (func (export "f") (param i32) local.get 0 call $deep0)"#,
                // The first is inlined into its caller, the others are merged.
                repeat(3, &|f| format!(
                    r#"(func $deep{f} {} (param i32) (local i32) {}local.get 0 i32.const {} i32.add local.tee 1 call $log local.get 1 call $log{})"#,
                    if f > 0 {
                        format!(r#"(export "deep{f}")"#)
                    } else {
                        String::new()
                    },
                    "local.get 0 if ".repeat(498),
                    1000 + f,
                    " end".repeat(498),
                )),
            ),
        ),
        (
            "a long run of calls and sets",
            &[Pass::Stackify],
            format!(
                r#"(func $g (result i32) i32.const 1) (func (export "f") (result i32) (local {}) {} {} i32.const 0)"#,
                "i32 ".repeat(20_000),
                repeat(20_000, &|i| format!("call $g local.set {i} ")),
                repeat(20_000, &|i| format!("local.get {i} drop ")),
            ),
        ),
    ];
    for (name, passes, module) in cases {
        let wasm = wat::parse_str(format!("(module {module})")).unwrap();
        let start = std::time::Instant::now();
        let out = wasm_shrink_fuzz::shrink(&wasm, passes, false, false).unwrap();
        let time = start.elapsed();
        if let Err(e) = wasm_shrink_fuzz::validate(&out.wasm) {
            panic!("{name}: {e}");
        }
        assert!(time.as_secs() < 30, "{name}: took {time:?}");
    }
}
