//! Function reordering, applied to the emitted module (binaryen's
//! `reorder-functions`, which `-Oz` does not run).
//!
//! A `call` encodes its target's index as a LEB128: one byte below 128, two
//! below 16384, then three. walrus emits functions largest first, which gives
//! the small, often-called ones (`memcpy`, `__rust_dealloc`, panics) the
//! highest indices. This renumbers the defined functions so the most-used
//! come first. Imports keep their indices: they always precede definitions.
//!
//! Every function index in the module goes through the same permutation
//! (calls, `ref.func`, element segments, exports, the start function, the
//! name section), so the module means the same. Only the numbering and the
//! order of bodies in the code section change. That moves code offsets, so
//! don't run it on a module with DWARF you want to keep.

use std::convert::Infallible;
use wasm_encoder::reencode::{Error, Reencode, utils};
use wasmparser::{ElementItems, ExternalKind, Operator, Parser, Payload, TypeRef};

/// How often each defined function's index is written, and how many
/// functions are imported.
fn count_uses(wasm: &[u8]) -> wasmparser::Result<(u32, Vec<u64>)> {
    let mut imported = 0u32;
    let mut uses: Vec<u64> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let expr = |e: wasmparser::ConstExpr, indices: &mut Vec<u32>| -> wasmparser::Result<()> {
        for op in e.get_operators_reader() {
            if let Operator::RefFunc { function_index } = op? {
                indices.push(function_index);
            }
        }
        Ok(())
    };
    for payload in Parser::new(0).parse_all(wasm) {
        match payload? {
            Payload::ImportSection(s) => {
                for imp in s.into_imports() {
                    if matches!(imp?.ty, TypeRef::Func(_)) {
                        imported += 1;
                    }
                }
            }
            Payload::FunctionSection(s) => uses.resize(s.count() as usize, 0),
            Payload::GlobalSection(s) => {
                for g in s {
                    expr(g?.init_expr, &mut indices)?;
                }
            }
            Payload::ExportSection(s) => {
                for e in s {
                    let e = e?;
                    if e.kind == ExternalKind::Func {
                        indices.push(e.index);
                    }
                }
            }
            Payload::StartSection { func, .. } => indices.push(func),
            Payload::ElementSection(s) => {
                for el in s {
                    match el?.items {
                        ElementItems::Functions(fs) => {
                            for f in fs {
                                indices.push(f?);
                            }
                        }
                        ElementItems::Expressions(_, es) => {
                            for e in es {
                                expr(e?, &mut indices)?;
                            }
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                for op in body.get_operators_reader()? {
                    match op? {
                        Operator::Call { function_index }
                        | Operator::ReturnCall { function_index }
                        | Operator::RefFunc { function_index } => indices.push(function_index),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    for i in indices {
        if let Some(n) = i
            .checked_sub(imported)
            .and_then(|i| uses.get_mut(i as usize))
        {
            *n += 1;
        }
    }
    Ok((imported, uses))
}

fn leb_len(v: u32) -> u64 {
    match v {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        0x4000..0x20_0000 => 3,
        0x20_0000..0x1000_0000 => 4,
        _ => 5,
    }
}

/// Re-encodes a module with its defined functions renumbered.
struct Renumber {
    imported: u32,
    /// New position of each defined function, by old position.
    new: Vec<u32>,
    /// Old position of each defined function, by new position.
    old: Vec<u32>,
}

impl Reencode for Renumber {
    type Error = Infallible;

    fn function_index(&mut self, f: u32) -> Result<u32, Error> {
        Ok(match f.checked_sub(self.imported) {
            Some(i) => self.imported + self.new[i as usize],
            None => f,
        })
    }

    fn parse_function_section(
        &mut self,
        functions: &mut wasm_encoder::FunctionSection,
        section: wasmparser::FunctionSectionReader<'_>,
    ) -> Result<(), Error> {
        let types: Vec<u32> = section.into_iter().collect::<Result<_, _>>()?;
        for i in 0..self.old.len() {
            functions.function(self.type_index(types[self.old[i] as usize])?);
        }
        Ok(())
    }

    fn parse_code_section(
        &mut self,
        code: &mut wasm_encoder::CodeSection,
        section: wasmparser::CodeSectionReader<'_>,
    ) -> Result<(), Error> {
        let bodies: Vec<wasmparser::FunctionBody> =
            section.into_iter().collect::<Result<_, _>>()?;
        for i in 0..self.old.len() {
            let body = bodies[self.old[i] as usize].clone();
            self.parse_function_body(code, body)?;
        }
        Ok(())
    }

    fn parse_custom_name_subsection(
        &mut self,
        names: &mut wasm_encoder::NameSection,
        section: wasmparser::Name<'_>,
    ) -> Result<(), Error> {
        // Name maps are sorted by index, so these three are rebuilt in the
        // new order; the rest don't mention functions.
        let indirect = |this: &mut Self, map: wasmparser::IndirectNameMap<'_>| {
            let mut entries = Vec::new();
            for n in map {
                let n = n?;
                let inner = utils::name_map(n.names, Ok::<_, Error>)?;
                entries.push((this.function_index(n.index)?, inner));
            }
            entries.sort_by_key(|e| e.0);
            let mut out = wasm_encoder::IndirectNameMap::new();
            for (i, inner) in &entries {
                out.append(*i, inner);
            }
            Ok::<_, Error>(out)
        };
        match section {
            wasmparser::Name::Function(map) => {
                let mut entries = Vec::new();
                for n in map {
                    let n = n?;
                    entries.push((self.function_index(n.index)?, n.name));
                }
                entries.sort_by_key(|e| e.0);
                let mut out = wasm_encoder::NameMap::new();
                for (i, name) in entries {
                    out.append(i, name);
                }
                names.functions(&out);
            }
            wasmparser::Name::Local(map) => names.locals(&indirect(self, map)?),
            wasmparser::Name::Label(map) => names.labels(&indirect(self, map)?),
            other => utils::parse_custom_name_subsection(self, names, other)?,
        }
        Ok(())
    }
}

/// Returns the module with its defined functions renumbered most-used first,
/// and how many bytes of function indices that saves. Functions used equally
/// often keep their order.
///
/// The module comes back unchanged (and the saving is 0) when no order is
/// smaller, which is the case when it imports 128 functions or more and has
/// fewer than 16384 in all.
///
/// Code offsets move: don't run this on a module with DWARF you want to keep.
pub fn reorder_functions(wasm: &[u8]) -> walrus::Result<(Vec<u8>, usize)> {
    let (imported, uses) = count_uses(wasm)?;
    let mut old: Vec<u32> = (0..uses.len() as u32).collect();
    old.sort_by_key(|&i| (std::cmp::Reverse(uses[i as usize]), i));
    let cost = |order: &mut dyn Iterator<Item = u32>| -> u64 {
        order
            .enumerate()
            .map(|(at, i)| uses[i as usize] * leb_len(imported + at as u32))
            .sum()
    };
    let before = cost(&mut (0..uses.len() as u32));
    let after = cost(&mut old.iter().copied());
    if after >= before {
        return Ok((wasm.to_vec(), 0));
    }
    let mut new = vec![0; old.len()];
    for (at, &i) in old.iter().enumerate() {
        new[i as usize] = at as u32;
    }
    let mut module = wasm_encoder::Module::new();
    Renumber { imported, new, old }.parse_core_module(&mut module, Parser::new(0), wasm)?;
    Ok((module.finish(), (before - after) as usize))
}

#[cfg(test)]
mod tests {
    use super::reorder_functions;
    use crate::test_util::{function_text, validate};

    /// One import, 200 small functions and `$hot`, which is defined last and
    /// called from all of them. `$f10` is also exported, in a table, the
    /// start function and the target of a `ref.func`.
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

    /// The names in `(func $name …` order.
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
        let (out, saved) = reorder_functions(&wasm).unwrap();
        validate(&out);
        // `$hot` was index 201 (two bytes) in 200 calls; now it is 1.
        assert!(saved >= 200, "{saved}");
        assert_eq!(wasm.len() - out.len(), saved);
        let names = order(&out);
        assert_eq!(&names[..2], ["hot", "f10"]);
        // Everything else keeps its order.
        let rest: Vec<String> = (0..200)
            .filter(|&i| i != 10)
            .map(|i| format!("f{i}"))
            .collect();
        assert_eq!(&names[2..], &rest[..]);
    }

    #[test]
    fn every_reference_follows_its_function() {
        let wasm = module();
        let (out, _) = reorder_functions(&wasm).unwrap();
        // Calls, `ref.func` and local names are printed by name, so the
        // bodies read the same before and after.
        for f in ["f0", "f10", "f199", "hot"] {
            assert_eq!(function_text(&out, f), function_text(&wasm, f), "{f}");
        }
        let text = |w: &[u8]| {
            let t = wasmprinter::print_bytes(w).unwrap();
            let keep = ["(export", "(start", "(elem", "(global"];
            t.lines()
                .filter(|l| keep.iter().any(|k| l.trim_start().starts_with(k)))
                // `(;12;)` index comments differ; names must not.
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
        let (once, _) = reorder_functions(&module()).unwrap();
        let (twice, saved) = reorder_functions(&once).unwrap();
        assert_eq!(saved, 0);
        assert_eq!(once, twice);
    }

    #[test]
    fn leaves_a_module_with_nothing_to_gain() {
        // Every index fits one byte in any order.
        let wasm =
            wat::parse_str("(module (func $a call $b call $b) (func $b) (export \"a\" (func $a)))")
                .unwrap();
        let (out, saved) = reorder_functions(&wasm).unwrap();
        assert_eq!(saved, 0);
        assert_eq!(out, wasm);
    }
}
