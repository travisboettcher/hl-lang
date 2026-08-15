#![no_main]

use libfuzzer_sys::fuzz_target;

// Mirrors the parse -> compose -> generate chain in
// `crates/hl-cli/src/lib.rs`'s `build_yaml`, so this target exercises
// codegen (interpolation, network resolution, labels) on adversarial
// inputs that only `fuzz_parse` alone wouldn't reach.
fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data)
        && let Ok(program) = hl_parser::parse(source)
        && let Ok(composed) = hl_parser::compose(program)
    {
        let _ = hl_codegen::generate(composed);
    }
});
