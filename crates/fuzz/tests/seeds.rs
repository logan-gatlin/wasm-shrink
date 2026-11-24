use wasm_shrink_fuzz::{Checked, check, seed_data as data};

fn env(name: &str) -> Option<u64> {
    std::env::var(name).ok().map(|v| v.parse().expect(name))
}

#[test]
fn seeds() {
    let seeds = match env("FUZZ_SEED") {
        Some(s) => s..s + 1,
        None => {
            let start = env("FUZZ_START").unwrap_or(0);
            start..start + env("FUZZ_SEEDS").unwrap_or(200)
        }
    };
    let total = seeds.end - seeds.start;
    let mut executed = 0;
    let mut failed = Vec::new();
    for seed in seeds {
        if let Ok(path) = std::env::var("FUZZ_DUMP") {
            std::fs::write(path, data(seed)).unwrap();
        }
        let r = std::panic::catch_unwind(|| check(&data(seed)));
        match r {
            Ok(Checked::Executed) => executed += 1,
            Ok(_) => {}
            Err(_) => failed.push(seed),
        }
    }
    assert!(
        failed.is_empty(),
        "failing seeds (rerun one with FUZZ_SEED=n): {failed:?}"
    );
    assert!(
        total > 0 && executed * 2 >= total,
        "only {executed} of {total} cases ran to completion"
    );
}
