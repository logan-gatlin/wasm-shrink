use std::cmp::Reverse;
use walrus::{FunctionInfo, FunctionKind, Module, ModuleConfig};

fn leb_len(v: u64) -> u64 {
    match v {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        0x4000..0x20_0000 => 3,
        0x20_0000..0x1000_0000 => 4,
        _ => 5,
    }
}

fn cost(imported: u64, uses: impl Iterator<Item = u64>) -> u64 {
    (imported..).zip(uses).map(|(i, n)| n * leb_len(i)).sum()
}

pub fn most_used_first(module: &Module, functions: &mut [FunctionInfo]) -> u64 {
    let imported = module
        .funcs
        .iter()
        .filter(|f| matches!(f.kind, FunctionKind::Import(_)))
        .count() as u64;
    let mut uses: Vec<u64> = functions.iter().map(FunctionInfo::uses).collect();
    let before = cost(imported, uses.iter().copied());
    uses.sort_by_key(|&n| Reverse(n));
    let after = cost(imported, uses.into_iter());
    if after >= before {
        return 0;
    }
    functions.sort_by_key(|f| Reverse(f.uses()));
    before - after
}

pub fn reorder_functions(config: &mut ModuleConfig) -> &mut ModuleConfig {
    config.function_order(|module, functions| {
        most_used_first(module, functions);
    })
}

#[cfg(test)]
mod tests {
    use super::{most_used_first, reorder_functions};
    use crate::test_util::{function_text, validate};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use walrus::ModuleConfig;

    fn module() -> Vec<u8> {
        let mut wat = String::from(
            "(module (import \"env\" \"log\" (func $log (param i32)))
               (table 2 funcref) (elem (i32.const 0) $f10 $hot)
               (global $g funcref (ref.func $f10))
               (export \"f10\" (func $f10)) (export \"hot\" (func $hot))
               (start $f10)\n",
        );
        for i in 0..200 {
            wat += &format!(
                "(func $f{i} (local $x i32) i32.const {i} call $hot local.get $x call $log)\n"
            );
        }
        wat += "(func $hot (param $p i32) local.get $p call $log
                  (drop (ref.func $f10))))";
        wat::parse_str(&wat).unwrap()
    }

    fn plain(wasm: &[u8]) -> Vec<u8> {
        ModuleConfig::new().parse(wasm).unwrap().emit_wasm()
    }

    fn reorder(wasm: &[u8]) -> (Vec<u8>, u64) {
        let saved = Arc::new(AtomicU64::new(0));
        let record = saved.clone();
        let mut config = ModuleConfig::new();
        config.function_order(move |m, f| record.store(most_used_first(m, f), Ordering::Relaxed));
        let out = config.parse(wasm).unwrap().emit_wasm();
        validate(&out);
        (out, saved.load(Ordering::Relaxed))
    }

    fn order(wasm: &[u8]) -> Vec<String> {
        wasmprinter::print_bytes(wasm)
            .unwrap()
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("(func $"))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn most_used_function_comes_first() {
        let wasm = module();
        let (out, saved) = reorder(&wasm);
        assert!(saved >= 200, "{saved}");
        assert_eq!(plain(&wasm).len() - out.len(), saved as usize);
        let names = order(&out);
        assert_eq!(&names[..2], ["hot", "f10"]);
        let rest: Vec<String> = (0..200)
            .filter(|&i| i != 10)
            .map(|i| format!("f{i}"))
            .collect();
        assert_eq!(&names[2..], &rest[..]);
    }

    #[test]
    fn every_reference_follows_its_function() {
        let wasm = plain(&module());
        let (out, _) = reorder(&wasm);
        for f in ["f0", "f10", "f199", "hot"] {
            assert_eq!(function_text(&out, f), function_text(&wasm, f), "{f}");
        }
        let text = |w: &[u8]| {
            let t = wasmprinter::print_bytes(w).unwrap();
            let keep = ["(export", "(start", "(elem", "(global"];
            t.lines()
                .filter(|l| keep.iter().any(|k| l.trim_start().starts_with(k)))
                .map(|l| {
                    l.split(' ')
                        .filter(|w| !w.starts_with("(;"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&out), text(&wasm));
    }

    #[test]
    fn is_stable() {
        let (once, _) = reorder(&module());
        let (twice, saved) = reorder(&once);
        assert_eq!(saved, 0);
        assert_eq!(once, twice);
    }

    #[test]
    fn leaves_a_module_with_nothing_to_gain() {
        let wasm =
            wat::parse_str("(module (func $a call $b call $b) (func $b) (export \"a\" (func $a)))")
                .unwrap();
        let (out, saved) = reorder(&wasm);
        assert_eq!(saved, 0);
        assert_eq!(out, plain(&wasm));
    }

    #[test]
    fn installs_on_a_config() {
        let wasm = module();
        let mut config = ModuleConfig::new();
        reorder_functions(&mut config);
        let out = config.parse(&wasm).unwrap().emit_wasm();
        assert_eq!(out, reorder(&wasm).0);
    }
}
