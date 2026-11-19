use wasm_shrink_fuzz::eh::{self, EhCase};
use wasm_shrink_fuzz::seed_data;

fn env(name: &str) -> Option<u64> {
    std::env::var(name).ok().map(|v| v.parse().expect(name))
}

#[test]
fn eh_seeds() {
    if !eh::node_available() {
        eprintln!("node not found: skipping the exception-handling seeds");
        return;
    }
    let seeds = match env("FUZZ_SEED") {
        Some(s) => s..s + 1,
        None => {
            let start = env("FUZZ_START").unwrap_or(0);
            start..start + env("FUZZ_SEEDS").unwrap_or(300)
        }
    };
    let mut failed = Vec::new();
    for seed in seeds {
        let Ok(case) = EhCase::generate(&seed_data(seed)) else {
            continue;
        };
        if let Err(e) = eh::check_case(&case) {
            failed.push(format!(
                "seed {seed} {}: {e}",
                eh::passes_flag(&case.passes)
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} failing seeds (rerun one with FUZZ_SEED=n):\n{}",
        failed.len(),
        failed
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
