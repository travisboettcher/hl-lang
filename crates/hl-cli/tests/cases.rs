//! File-driven regression corpus: one `.hll` file per case, with what
//! the compiler must make of it recorded beside it as plain text.
//! Adding a regression is dropping in a file and blessing the result —
//! no Rust to write, no test function to name, no expected string to
//! paste into a literal.
//!
//! # What a case pins down
//!
//! Each case runs the pipeline `hllc --build` runs — [`hl_linker::link`]
//! (lex, parse, compose, resolve `use` imports) then
//! [`hl_codegen::generate`] — and records the outcome in its `.expected`
//! file as labelled sections:
//!
//! - `--- error ---`, the rendered diagnostic, when the pipeline fails;
//! - `--- warnings ---` (only when there are any) followed by
//!   `--- compose ---`, the generated document, when it succeeds.
//!
//! Both paths carry equal weight. A diagnostic is text a user reads, and
//! it reviews far better as text in a file than as an escaped Rust
//! string literal — the argument `tests/cmd/` already makes for the CLI's
//! output, applied one layer down to the compiler itself. The sections
//! are always labelled, including the single-section cases, so a case
//! that flips from compiling to erroring shows up as a changed label
//! rather than as a wholesale content swap.
//!
//! `hl_cli::GENERATED_HEADER` is deliberately *not* part of a case. It
//! belongs to the `hllc` command rather than to the language, and
//! `tests/cmd/build-out.trycmd` already pins it.
//!
//! # Why this exists next to the hand-written suites
//!
//! This is the on-ramp for new cases, not a replacement for anything.
//! The existing suites carry doc comments explaining why each case
//! exists, and that rationale is worth more than uniformity, so nothing
//! was migrated into here. What a case file carries instead is its own
//! rationale, in the `#` comment block it has to open with:
//! [`require_rationale`] fails a case whose first line is not a comment.
//! That is what keeps "why is this pinned?" answerable in a directory of
//! inputs, where there is no surrounding Rust doc comment to put it in.
//!
//! # Blessing
//!
//! Expectations are [`snapbox`] file assertions, which is the same
//! machinery `trycmd` uses for `tests/cmd/`, driven by the same
//! variable — so the corpus adds no new way to update expected output:
//!
//! ```sh
//! SNAPSHOTS=overwrite cargo test -p hl-cli --test cases
//! ```
//!
//! CI pins `SNAPSHOTS=verify`, so it can no more bless a case here than
//! it can rewrite a transcript. Read the resulting diff before
//! committing it: the `.expected` file *is* the assertion, so rewriting
//! one declares that the new YAML or the new diagnostic is what the
//! compiler should now produce.

use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use hl_linker::FsLoader;
use snapbox::{Assert, Data, assert::DEFAULT_ACTION_ENV, report::Palette};

/// A case failure, carrying text already formatted for a reader.
///
/// `datatest-stable` renders a failed case with `{:?}`, so [`fmt::Debug`]
/// is what a reader actually sees. Deriving it would escape a
/// multi-line diff into a single quoted line — unreadable exactly when
/// it matters most — so `Debug` here prints the message as-is.
struct Failure(String);

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failure {}

/// A case this repo owns, under `tests/cases/`, named for what it pins
/// down.
///
/// `datatest-stable` prefixes each generated test with this function's
/// name, so `issue_168_healthcheck_param.hll` becomes the test
/// `regression_case::issue_168_healthcheck_param.hll`. That is what
/// makes `cargo test issue_168` select one case and what makes a
/// failure name the file — the reason for a custom harness rather than
/// a single `#[test]` walking the directory itself.
fn regression_case(path: &Path) -> datatest_stable::Result<()> {
    require_rationale(path)?;
    check(path, path.with_extension("expected"))
}

/// A case seeded from `crates/hl-parser/tests/fixtures/`, read where it
/// lives rather than copied in.
///
/// Those files are inputs several suites already share. Duplicating
/// them into the corpus would create two sources of truth for the same
/// input, so the corpus borrows them — and because they are owned by
/// those suites rather than by this one, the rationale rule
/// [`regression_case`] enforces does not apply. Their expectations
/// still land under `tests/cases/fixtures/`, with the rest of the
/// corpus, rather than beside the sources: a stray `.expected` in a
/// fixture directory would be swept up by anything globbing it, such as
/// the fuzz corpus seeding in `.github/workflows/ci.yml`.
fn fixture_case(path: &Path) -> datatest_stable::Result<()> {
    let name = path.with_extension("expected");
    let name = name.file_name().unwrap_or_default();
    check(path, Path::new("tests/cases/fixtures").join(name))
}

/// Fails a case file that does not open with a `#` comment.
///
/// A directory of inputs has no natural home for *why* a case exists —
/// the objection that keeps the hand-written suites hand-written. `.hll`
/// has line comments, so the case file itself is that home, and making
/// it a hard failure rather than a documented nicety is what stops the
/// corpus decaying into a pile of anonymous inputs as it grows.
fn require_rationale(path: &Path) -> datatest_stable::Result<()> {
    if !std::fs::read_to_string(path)?.starts_with('#') {
        return Err(Failure(format!(
            "{}: a case file must open with a `#` comment saying what it pins down — \
             the issue it regressed, or the behavior it locks in",
            path.display()
        ))
        .into());
    }
    Ok(())
}

/// Compiles one case and compares the result with its `.expected` file.
///
/// The mismatch comes back as an `Err` rather than a panic so that
/// `libtest-mimic` prints it under this case's own name. Panicking
/// instead would let the diff race the harness's progress lines onto
/// stdout, which is what makes an eight-line diff unreadable.
fn check(path: &Path, expected: std::path::PathBuf) -> datatest_stable::Result<()> {
    let assert = Assert::new()
        .action_env(DEFAULT_ACTION_ENV)
        // The diff is built into a `String` rather than written to a
        // stream, so `anstream`'s own terminal detection never gets to
        // strip the escapes — leaving them in would litter the output of
        // any run that is piped or captured, CI's included.
        .palette(if std::io::stderr().is_terminal() {
            Palette::color()
        } else {
            Palette::plain()
        });
    assert
        .try_eq(
            Some(&path.display()),
            compile(path).into(),
            Data::read_from(&expected, None),
        )
        .map_err(|err| Failure(err.to_string()))?;
    Ok(())
}

/// Runs the `hllc --build` pipeline over one case and renders the
/// outcome as the sectioned text its `.expected` file holds.
fn compile(path: &Path) -> String {
    let rendered = match hl_linker::link(path, &FsLoader) {
        Err(err) => format!("--- error ---\n{err}\n"),
        Ok(linked) => {
            let files = linked.program.files.clone();
            let mut warnings: Vec<String> = linked
                .warnings
                .iter()
                .map(|warning| warning.display(&files).to_string())
                .collect();
            match hl_codegen::generate(linked.program) {
                // Warnings raised before the failure are dropped on
                // purpose: what this case pins down is the diagnostic
                // that stopped the build.
                Err(err) => format!("--- error ---\n{}\n", err.display(&files)),
                Ok(generated) => {
                    warnings.extend(
                        generated
                            .warnings
                            .iter()
                            .map(|warning| warning.display(&files).to_string()),
                    );
                    let mut out = String::new();
                    if !warnings.is_empty() {
                        out.push_str("--- warnings ---\n");
                        for warning in &warnings {
                            out.push_str(warning);
                            out.push('\n');
                        }
                    }
                    out.push_str("--- compose ---\n");
                    out.push_str(&generated.yaml);
                    out
                }
            }
        }
    };
    normalize_paths(&rendered, path)
}

/// Rewrites the case's own path down to its file name wherever a
/// diagnostic quotes it.
///
/// Diagnostics are prefixed `path:line:col`, and the path they print is
/// the one handed to [`hl_linker::link`] — which differs per corpus root
/// (`tests/cases/x.hll` against `../hl-parser/tests/fixtures/x.hll`).
/// Recording that would pin where a case happens to live rather than
/// what it says.
fn normalize_paths(rendered: &str, path: &Path) -> String {
    let full = path.to_string_lossy();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    rendered.replace(full.as_ref(), name.as_ref())
}

datatest_stable::harness! {
    { test = regression_case, root = "tests/cases", pattern = r"\.hll$" },
    { test = fixture_case, root = "../hl-parser/tests/fixtures", pattern = r"\.hll$" },
}
