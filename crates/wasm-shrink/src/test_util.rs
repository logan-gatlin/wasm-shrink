//! Helpers for the passes' unit tests.

use walrus::Module;

/// The module as walrus emits it, validated.
pub fn emit(m: &mut Module) -> Vec<u8> {
    let wasm = m.emit_wasm();
    validate(&wasm);
    wasm
}

pub fn validate(wasm: &[u8]) {
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(wasm)
        .expect("the output validates");
}

/// The text of function `$name` in `wasm`: its local declarations and
/// instructions, comments dropped, separated by single spaces.
pub fn function_text(wasm: &[u8], name: &str) -> String {
    let text = wasmprinter::print_bytes(wasm).unwrap();
    let head = format!("(func ${name} ");
    let mut lines = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with(&head));
    let indent = |l: &str| l.len() - l.trim_start().len();
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("no function ${name} in:\n{text}"));
    let mut words = Vec::new();
    for line in lines.take_while(|l| indent(l) > indent(first)) {
        // `;; label = @1` runs to the end of the line, `(;@1;)` is inline.
        let line = line.split(";;").next().unwrap();
        words.extend(
            line.split_whitespace()
                .filter(|w| !(w.starts_with("(;") && w.ends_with(";)")))
                .map(str::to_string),
        );
    }
    words.join(" ")
}

/// Parse `wat`, apply `pass`, and return the text of function `$f` (see
/// [`function_text`]).
pub fn run(wat: &str, pass: impl FnOnce(&mut Module)) -> String {
    let wasm = wat::parse_str(wat).expect("the test module parses");
    validate(&wasm);
    let mut m = Module::from_buffer(&wasm).expect("walrus reads the test module");
    pass(&mut m);
    function_text(&emit(&mut m), "f")
}

/// `text` with runs of whitespace collapsed, to compare against [`run`].
pub fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
