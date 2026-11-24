//! wasm-shrink: the size-reducing parts of `wasm-opt -Oz`, as walrus passes.
//!
//! Usage: wasm-shrink IN.wasm -o OUT.wasm [--passes a,b,c] [--translate-to-exnref] [--no-reorder] [--quiet]
//! (`--help` lists every option and pass.)
//!
//! With no `--passes`, the default pipeline runs. `--passes none` only
//! round-trips the module through walrus (the baseline for measurements).
//! The module is emitted with its most-used functions first (see
//! [`wasm_shrink::most_used_first`]); `--no-reorder` keeps walrus's order.
//! `--translate-to-exnref` first rewrites legacy exception handling as
//! `try_table`/`throw_ref` (see [`wasm_shrink::passes::exnref`]).

use anyhow::{Context, Result};
use clap::Parser;
use clap::builder::PossibleValuesParser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use wasm_shrink::Pass;

/// The size-reducing parts of `wasm-opt -Oz`, as walrus passes.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Module to shrink.
    input: PathBuf,
    /// Where to write the result.
    #[arg(short, long)]
    output: PathBuf,
    /// Passes to run, in order, instead of the default pipeline. `none` only
    /// round-trips the module through walrus.
    #[arg(long, value_delimiter = ',', value_parser = pass_names())]
    passes: Option<Vec<String>>,
    /// Rewrite legacy exception handling (`try`, `delegate`, `rethrow`) as
    /// `try_table` and `throw_ref` before the other passes.
    #[arg(long)]
    translate_to_exnref: bool,
    /// Keep walrus's function order (largest first).
    #[arg(long)]
    no_reorder: bool,
    /// Do not print per-pass timings and stats.
    #[arg(short, long)]
    quiet: bool,
}

/// Every pass name, plus `none`.
fn pass_names() -> PossibleValuesParser {
    let mut names: Vec<&str> = Pass::ALL.iter().map(|p| p.name()).collect();
    names.sort_unstable();
    names.push("none");
    PossibleValuesParser::new(names)
}

fn main() -> Result<()> {
    let Args {
        input,
        output,
        passes: list,
        translate_to_exnref,
        no_reorder,
        quiet,
    } = Args::parse();
    let wasm = std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
    let mut config = walrus::ModuleConfig::new();
    let reorder = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    if !no_reorder {
        let record = reorder.clone();
        config.function_order(move |m, functions| {
            let t = Instant::now();
            let saved = wasm_shrink::most_used_first(m, functions);
            record[0].store(saved, Ordering::Relaxed);
            record[1].store(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        });
    }
    let mut m = config.parse(&wasm).context("parsing module")?;

    let mut passes: Vec<Pass> = match list {
        None => Pass::DEFAULT.to_vec(),
        Some(l) => l
            .iter()
            .filter(|p| *p != "none")
            .map(|p| p.parse())
            .collect::<Result<_, _>>()?,
    };
    if translate_to_exnref {
        passes.insert(0, Pass::TranslateToExnref);
    }
    wasm_shrink::gc(&mut m);
    for pass in passes {
        let t = Instant::now();
        let stats = pass.run(&mut m);
        if !quiet {
            eprintln!(
                "{:16} {:>7.1} ms  {stats}",
                pass.name(),
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
    wasm_shrink::gc(&mut m);
    let out = m.emit_wasm();
    if !no_reorder && !quiet {
        eprintln!(
            "{:16} {:>7.1} ms  saved {} bytes of function indices",
            "reorder",
            reorder[1].load(Ordering::Relaxed) as f64 / 1e6,
            reorder[0].load(Ordering::Relaxed),
        );
    }
    if !quiet {
        eprintln!(
            "{} -> {} bytes ({:+.2}%)",
            wasm.len(),
            out.len(),
            (out.len() as f64 / wasm.len() as f64 - 1.0) * 100.0
        );
    }
    std::fs::write(&output, out).with_context(|| format!("writing {}", output.display()))?;
    Ok(())
}
