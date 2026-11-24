use std::process::ExitCode;
use wasm_shrink::Pass;
use wasm_shrink_fuzz::eh::{self, EhCase};
use wasm_shrink_fuzz::{Case, check_case};

const USAGE: &str = "usage:
  fuzz-case decode DATA -o IN.wasm      write the module a fuzz input generates and print its flags
  fuzz-case seeds DIR N                 write the seeded test's first N inputs to DIR
  fuzz-case emit IN.wasm PASSES -o OUT.wasm
                                        write the module after PASSES (no reorder)
  fuzz-case coverage N                  count how the first N seeded inputs end, and the ones each pass changes
  fuzz-case eh SEED|DATA -o IN.wasm     write the exception-handling case a seed or fuzz input generates
                                        and print its passes
  fuzz-case eh-check IN.wasm --passes a,b
                                        exit 1 if node sees the output behave differently
  fuzz-case eh-compare A.wasm B.wasm    exit 1 if node sees A and B behave differently
  fuzz-case check IN.wasm [--passes a,b] [--no-reorder] [--args FILE]
                                        exit 1 if a property fails";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    match args {
        [cmd, data, o, out] if cmd == "decode" && o == "-o" => {
            let case = Case::generate(&read(data)?).map_err(|e| e.to_string())?;
            std::fs::write(out, &case.wasm).map_err(|e| format!("{out}: {e}"))?;
            std::fs::write(format!("{out}.args"), &case.args).map_err(|e| e.to_string())?;
            println!("{case} --args {out}.args");
            Ok(ExitCode::SUCCESS)
        }
        [cmd, dir, n] if cmd == "seeds" => {
            let n: u64 = n.parse().map_err(|_| format!("bad count {n}"))?;
            std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
            for seed in 0..n {
                let path = format!("{dir}/seed-{seed}");
                std::fs::write(&path, wasm_shrink_fuzz::seed_data(seed))
                    .map_err(|e| format!("{path}: {e}"))?;
            }
            Ok(ExitCode::SUCCESS)
        }
        [cmd, input, o, out] if cmd == "eh" && o == "-o" => {
            let data = match input.parse::<u64>() {
                Ok(seed) => wasm_shrink_fuzz::seed_data(seed),
                Err(_) => read(input)?,
            };
            let case = EhCase::generate(&data).map_err(|e| e.to_string())?;
            std::fs::write(out, &case.wasm).map_err(|e| format!("{out}: {e}"))?;
            println!("{}", eh::passes_flag(&case.passes));
            Ok(ExitCode::SUCCESS)
        }
        [cmd, wasm, p, passes] if cmd == "eh-check" && p == "--passes" => {
            let case = EhCase {
                wasm: read(wasm)?,
                passes: parse_passes(passes)?,
            };
            verdict(eh::check_case(&case))
        }
        [cmd, a, b] if cmd == "eh-compare" => verdict(eh::compare(&read(a)?, &read(b)?)),
        [cmd, n] if cmd == "coverage" => {
            let n: u64 = n.parse().map_err(|_| format!("bad count {n}"))?;
            let mut hits = vec![0; Pass::ALL.len()];
            let mut cases = 0;
            let mut ends = std::collections::BTreeMap::new();
            for seed in 0..n {
                let Ok(case) = Case::generate(&wasm_shrink_fuzz::seed_data(seed)) else {
                    continue;
                };
                cases += 1;
                let end = match check_case(&case) {
                    Ok(c) => format!("{c:?}"),
                    Err(_) => "Failed".into(),
                };
                *ends.entry(end).or_insert(0) += 1;
                for p in wasm_shrink_fuzz::changed_by(&case.wasm) {
                    hits[Pass::ALL.iter().position(|&q| q == p).unwrap()] += 1;
                }
            }
            println!("{cases} cases");
            for (end, count) in ends {
                println!("{count:>6} {end}");
            }
            println!("changed by");
            for (p, h) in Pass::ALL.iter().zip(hits) {
                println!("{:>6} {p}", h);
            }
            Ok(ExitCode::SUCCESS)
        }
        [cmd, wasm, passes, o, out] if cmd == "emit" && o == "-o" => {
            let passes = parse_passes(passes)?;
            let shrunk = wasm_shrink_fuzz::shrink(&read(wasm)?, &passes, false, false)
                .ok_or("walrus can't parse the module")?;
            std::fs::write(out, shrunk.wasm).map_err(|e| format!("{out}: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
        [cmd, wasm, rest @ ..] if cmd == "check" => {
            let mut case = Case {
                wasm: read(wasm)?,
                passes: Pass::DEFAULT.to_vec(),
                reorder: true,
                args: Vec::new(),
            };
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--no-reorder" => case.reorder = false,
                    "--passes" => {
                        case.passes = parse_passes(rest.next().ok_or("--passes needs a list")?)?;
                    }
                    "--args" => case.args = read(rest.next().ok_or("--args needs a file")?)?,
                    other => return Err(format!("unknown flag {other}")),
                }
            }
            match check_case(&case) {
                Ok(c) => {
                    println!("{c:?}");
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    println!("{e}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        _ => Err("bad arguments".into()),
    }
}

fn verdict(r: Result<wasm_shrink_fuzz::Checked, String>) -> Result<ExitCode, String> {
    match r {
        Ok(status) => {
            println!("{status:?}");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("{e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn parse_passes(list: &str) -> Result<Vec<Pass>, String> {
    list.split(',')
        .filter(|p| !p.is_empty() && *p != "none")
        .map(|p| p.parse::<Pass>().map_err(|e| e.to_string()))
        .collect()
}
