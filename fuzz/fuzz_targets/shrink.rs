#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    wasm_shrink_fuzz::check(data);
});
