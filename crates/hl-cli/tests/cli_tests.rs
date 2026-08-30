//! Transcript tests for the `hllc` command-line surface: the exact text
//! a user sees on stdout and stderr, together with the exit code, kept
//! as plain-text transcripts under `tests/cmd/`.
//!
//! These complement `build_tests.rs` rather than replacing it. A
//! transcript can only observe what the process itself observes — the
//! rendered output and the exit status — so every assertion about a
//! return value or about the filesystem *not* holding a file stays an
//! in-process `run()` test over in `build_tests.rs`. What moved here is
//! the set of tests that shelled out to the real binary precisely
//! because printed text was the only observable effect they had: those
//! were string comparisons written in Rust against output no file in
//! the repo showed as text.
//!
//! Each `tests/cmd/<name>.trycmd` may carry two sibling directories.
//! `<name>.in/` is copied into a sandbox and becomes the working
//! directory for every command in that transcript, which is what keeps
//! the diagnostics' `path:line:col` prefixes relative and therefore
//! stable across machines. `<name>.out/` is the expected state of that
//! sandbox afterwards, compared by `snapbox`'s directory diff — that is
//! how the multi-file `build --out <dir>` cases assert what was
//! written, instead of reading each generated file back by hand.
//!
//! Bless a deliberate change with `TRYCMD=overwrite cargo test -p
//! hl-cli --test cli_tests`, then read every line of the resulting diff
//! before committing it: the transcript *is* the assertion, so a
//! rewritten one declares that the new output is what `hllc` should
//! now print. CI leaves `TRYCMD` at its non-overwriting default, so a
//! mismatch there fails the build.

/// Resolving the binary through `CARGO_BIN_EXE_hllc` rather than
/// letting `trycmd` search the target directory for a file named
/// `hllc`: Cargo guarantees this variable points at the binary built
/// for *this* test run, which is the same guarantee `build_tests.rs`
/// relies on, and it can't accidentally pick up a stale binary from
/// another profile.
#[test]
fn cli_tests() {
    trycmd::TestCases::new()
        .register_bin("hllc", std::path::Path::new(env!("CARGO_BIN_EXE_hllc")))
        .case("tests/cmd/*.trycmd");
}
