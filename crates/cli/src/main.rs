//! wasm-shrink: the size-reducing parts of `wasm-opt -Oz`, as walrus passes.
//!
//! Usage: wasm-shrink IN.wasm -o OUT.wasm [--passes a,b,c] [--translate-to-exnref] [--no-reorder] [--quiet]
//! (`--help` lists every option and pass.)
//!
//! With no `--passes`, the default pipeline runs. `--passes none` only
//! round-trips the module through walrus (the baseline for measurements).
//! After emitting, the most-used functions are moved first (see
//! [`wasm_shrink::reorder_functions`]); `--no-reorder` skips that.
//! `--translate-to-exnref` first rewrites legacy exception handling as
//! `try_table`/`throw_ref` (see [`wasm_shrink::passes::exnref`]).

use anyhow::{Context, Result};
use clap::Parser;
use clap::builder::PossibleValuesParser;
use std::path::PathBuf;
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
    let mut m = walrus::ModuleConfig::new()
        .parse(&wasm)
        .context("parsing module")?;

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
    walrus::passes::gc::run(&mut m);
    let mut out = m.emit_wasm();
    if !no_reorder {
        let t = Instant::now();
        let (reordered, saved) =
            wasm_shrink::reorder_functions(&out).context("reordering functions")?;
        if !quiet {
            eprintln!(
                "{:16} {:>7.1} ms  saved {saved} bytes of function indices",
                "reorder",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
        out = reordered;
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
