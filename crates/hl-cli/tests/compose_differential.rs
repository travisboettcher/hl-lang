//! Differential test: every document `hllc` generates is fed to Docker
//! Compose's own parser, and Compose gets the last word on whether it
//! is valid (#172).
//!
//! Every other test in this workspace checks that codegen emits the
//! YAML *the repo expects it to emit*. The `insta` snapshots in
//! `hl-codegen`, the `trycmd` transcripts next door, the golden tests —
//! all of them compare output against an expectation written in this
//! repo by the same people who wrote the codegen. That proves the
//! output is stable, never that it is correct: a schema mistake that is
//! consistent between codegen and its recorded expectation passes every
//! one of them and fails for the first time on a user's machine at
//! `docker compose up`. An external oracle is the only thing that
//! closes that gap, and for a transpiler whose entire output contract
//! is "valid Docker Compose", Compose's own loader is that oracle.
//!
//! # Why this is behind a feature, and what happens when Docker is missing
//!
//! The whole file is `#[cfg(feature = "docker-differential")]`, off by
//! default. The alternative — an always-compiled `#[ignore]`d test —
//! was rejected for one reason: `cargo test -- --ignored` is an
//! all-or-nothing selector over the whole workspace, so it can't name
//! *this* suite without also sweeping in every other ignored test a
//! future contributor adds. A feature names exactly this corpus. The
//! cost of a feature is that gated code rots when nothing compiles it,
//! which `.github/workflows/ci.yml`'s `compose-differential` job pays
//! off: it builds and runs this file on every pull request, and the
//! `test` job's clippy step lints it with `--all-features`.
//!
//! When the feature *is* on and Docker is missing, this fails rather
//! than skipping. That is deliberate and it is the most important
//! decision in the file. `libtest` captures both stdout and stderr of a
//! passing test and prints neither, so the obvious "print why we
//! skipped and return" reads in a CI log as a green test that ran —
//! which is precisely the "skip that looks like a pass" the issue calls
//! worse than having no test at all. Enabling the feature *is* the
//! request to run the differential test, so not being able to run it is
//! a failure, and the panic message says exactly what is missing.
//!
//! # What Compose is asked
//!
//! `docker compose config` is a pure client-side load-and-validate: it
//! parses the file, applies Compose's own schema, and normalizes the
//! result. It needs no daemon and starts no containers, which is why
//! this test is cheap enough to run on every pull request.
//!
//! Each case is checked twice, deliberately:
//!
//! 1. `config --quiet` is the validity gate. Its stderr is the whole
//!    failure message, so a rejection is reported in Compose's words
//!    rather than paraphrased.
//! 2. `config` (same load, normalized document on stdout) feeds a
//!    structural comparison against the generated input. Compose
//!    accepting a document is not the same as Compose reading it the
//!    way the emitter meant it — a label whose backticks got mangled,
//!    an `expose` entry coerced between number and string, an
//!    environment value quoted into something else — and those are
//!    silent until deployment. Running the load twice costs one extra
//!    process per case and keeps the two assertions from ever being
//!    reported as each other.

#![cfg(feature = "docker-differential")]

mod book_blocks;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use book_blocks::extract_blocks;

/// Compose derives a project name from the compose file's parent
/// directory when none is given, which would make both the normalized
/// output and any error message depend on a scratch path containing a
/// process ID. Naming the project pins that down.
const PROJECT_NAME: &str = "hl-lang-differential";

/// Fixtures that are *supposed* not to build. `unknown_template.hll`
/// exists to make `ComposeError::UnknownTemplate` fire, so it never
/// reaches codegen and has no document to validate. Every other build
/// failure in the fixture directory is a hard failure here rather than
/// a skip — a fixture that silently stopped compiling would otherwise
/// drop out of this corpus without anyone noticing.
const NON_BUILDING_FIXTURES: &[&str] = &["unknown_template.hll"];

/// Rejections this repo knows about and has decided not to fix here.
///
/// `raw` is a documented escape hatch: its keys and values go to the
/// output verbatim, with no schema of the compiler's own applied. So a
/// `raw` body that Compose rejects is a bug in whoever wrote the `raw`
/// body — here, in the fixture — and not in codegen, which did exactly
/// what it promised. Failing this test on it would hold the differential
/// test hostage to fixture content that exists precisely to exercise the
/// unvalidated path.
///
/// Recording it as an entry rather than tolerating the whole category
/// keeps the hole exactly one case wide and visible in review. Each
/// entry is `(case name, substring Compose's stderr must contain)`,
/// matched on the offending key path rather than Compose's full
/// sentence so a reworded diagnostic doesn't turn this into a false
/// failure. A listed case that starts *passing* fails the test too, so
/// the list cannot outlive the problem it describes.
///
/// The list is empty, and empty is the state to keep it in. Its one
/// entry recorded `raw_service.hll` writing `security_opt` as a nested
/// map where Compose's schema demands a list of `option:value` strings;
/// #174 rewrote the fixture's `raw` body as that list, so the rejection
/// is gone and the entry went with it. The mechanism stays for the next
/// fixture that deliberately exercises the unvalidated path.
const KNOWN_REJECTIONS: &[(&str, &str)] = &[];

/// One document to put in front of Compose: a human-readable name for
/// failure messages, plus the generated YAML.
struct Case {
    name: String,
    yaml: String,
}

/// Confirms `docker compose` can be reached, or explains what is
/// missing. Probing with `compose version` rather than `docker version`
/// on purpose: the Compose v2 plugin is a separate binary from the
/// `docker` CLI, and only the plugin matters here.
fn require_docker_compose() {
    let probe = Command::new("docker").args(["compose", "version"]).output();
    let failure = match probe {
        Err(err) => format!("could not run `docker`: {err}"),
        Ok(out) if !out.status.success() => format!(
            "`docker compose version` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Ok(_) => return,
    };

    panic!(
        "\nthe `docker-differential` feature is enabled but Docker Compose is unavailable: \
         {failure}\n\n\
         This test asks Docker Compose to validate every document hllc generates, so \
         without it there is nothing to compare against. It fails instead of skipping \
         because libtest hides a passing test's output, which would leave a skip looking \
         like coverage in the CI log.\n\n\
         Install the Compose v2 plugin (https://docs.docker.com/compose/install/), or drop \
         `--features docker-differential` to run the rest of the suite without it. No Docker \
         daemon is needed — `docker compose config` only parses.\n"
    );
}

/// A scratch directory under the system temp dir, unique per test
/// process so parallel runs can't collide — the same approach
/// `build_tests.rs` takes, and for the same reason: it avoids a
/// `tempfile` dev-dependency for a one-off need.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hl-compose-differential-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("creating {}: {err}", dir.display()));
    dir
}

/// Runs the full pipeline — link (which composes) then codegen — over
/// one source file, exactly as `hllc --build` does.
fn build(src: &str) -> Result<String, String> {
    let mut loader = hl_linker::InMemoryLoader::default();
    loader.add("case.hll", src);
    let linked = hl_linker::link(Path::new("case.hll"), &loader).map_err(|err| err.to_string())?;
    hl_codegen::generate(linked.program)
        .map(|generated| generated.yaml)
        .map_err(|err| err.to_string())
}

/// Every `.hll` fixture under `crates/hl-parser/tests/fixtures/`,
/// read from the directory rather than listed here so a fixture added
/// later joins this corpus without anyone remembering to add it.
fn parser_fixture_cases() -> Vec<Case> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hl-parser/tests/fixtures");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "hll"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "found zero .hll fixtures under {} -- the path is probably wrong",
        dir.display()
    );

    let mut cases = Vec::new();
    for path in paths {
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        if NON_BUILDING_FIXTURES.contains(&file_name.as_str()) {
            continue;
        }
        let name = format!("hl-parser/tests/fixtures/{file_name}");
        let src = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        match build(&src) {
            Ok(yaml) => cases.push(Case { name, yaml }),
            Err(err) => panic!(
                "{name} failed to build, so Compose never saw it: {err}\n\
                 If that is deliberate, add the file to NON_BUILDING_FIXTURES with a reason."
            ),
        }
    }
    cases
}

/// The `,build`-tagged worked examples in `book/src/*.md` — the blocks
/// the book presents as complete, deployable services, and therefore
/// the ones a reader is most likely to copy into a real
/// `docker-compose.yml`.
///
/// Multi-file `group=` examples are deliberately not here. Every group
/// in the book today re-expresses, across several files, a service the
/// single-file corpus already covers (`imports.md`'s syncthing and
/// jellyfin examples), so running them would spend Docker invocations
/// re-validating documents this test has already validated. That stops
/// being true the moment a group example describes something new, which
/// is the point at which to widen this.
fn book_build_cases() -> Vec<Case> {
    let blocks = extract_blocks();
    assert!(
        !blocks.is_empty(),
        "found zero ```hll blocks in the book -- extraction is probably broken"
    );

    let mut cases = Vec::new();
    for block in &blocks {
        if !block.has("build") || block.has("ignore") {
            continue;
        }
        let name = format!("book/src/{}", block.location());
        match build(&block.code) {
            Ok(yaml) => cases.push(Case { name, yaml }),
            Err(err) => panic!(
                "{name} failed to build, so Compose never saw it: {err}\n\
                 book_examples.rs asserts the same thing and should have caught this first."
            ),
        }
    }
    assert!(
        !cases.is_empty(),
        "found zero ```hll,build blocks in the book -- the `build` tag may have been renamed"
    );
    cases
}

/// Asks Compose to load one generated document, then checks it read the
/// document the way codegen meant it. Returns one failure message per
/// problem found, so a run reports every bad case at once rather than
/// stopping at the first.
fn check(case: &Case, dir: &Path) -> Vec<String> {
    let file = dir.join("docker-compose.yml");
    fs::write(&file, &case.yaml).unwrap_or_else(|err| panic!("writing {}: {err}", file.display()));

    let compose = |extra: &[&str]| {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "--project-name", PROJECT_NAME, "--file"])
            .arg(&file)
            .arg("config")
            .args(extra);
        cmd.output()
            .unwrap_or_else(|err| panic!("running `docker compose config`: {err}"))
    };

    let known = KNOWN_REJECTIONS
        .iter()
        .find(|(name, _)| *name == case.name)
        .map(|(_, needle)| *needle);

    let validated = compose(&["--quiet"]);
    let stderr = String::from_utf8_lossy(&validated.stderr)
        .trim()
        .to_string();

    match (validated.status.success(), known) {
        // Compose rejected it, and the repo already knows why. Nothing
        // further to check: there is no normalized document to compare.
        (false, Some(needle)) if stderr.contains(needle) => return Vec::new(),
        (false, _) => {
            return vec![format!(
                "{}: `docker compose config` exited {} --\n{}\n{}",
                case.name,
                validated.status,
                indent(&stderr),
                indent(&case.yaml)
            )];
        }
        (true, Some(needle)) => {
            return vec![format!(
                "{}: Compose now accepts this, but KNOWN_REJECTIONS still lists it \
                 (expecting stderr to mention `{needle}`). Delete the entry.",
                case.name
            )];
        }
        (true, None) => {}
    }

    let normalized = compose(&[]);
    if !normalized.status.success() {
        return vec![format!(
            "{}: `docker compose config --quiet` passed but `docker compose config` exited {} --\n{}",
            case.name,
            normalized.status,
            indent(&String::from_utf8_lossy(&normalized.stderr))
        )];
    }

    let input: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&case.yaml) {
        Ok(value) => value,
        Err(err) => {
            return vec![format!(
                "{}: generated YAML does not parse: {err}",
                case.name
            )];
        }
    };
    let output: serde_yaml_ng::Value =
        match serde_yaml_ng::from_str(&String::from_utf8_lossy(&normalized.stdout)) {
            Ok(value) => value,
            Err(err) => {
                return vec![format!(
                    "{}: `docker compose config` output does not parse: {err}",
                    case.name
                )];
            }
        };

    compare(&case.name, &input, &output)
}

/// Compares the generated document against Compose's normalization of
/// it, on the axes where the two are supposed to agree.
///
/// Most of Compose's normalization is expected divergence and is not
/// checked: it stamps a project name on the document, attaches every
/// otherwise-unattached service to an implicit `default` network,
/// expands short-form volume strings into long-form mappings, and
/// rewrites list-shaped `labels`/`environment` into maps. Asserting
/// that away would mean reimplementing Compose's loader here, which is
/// the thing this test exists to avoid doing.
///
/// What it does check is the set of places where a rendering choice
/// could quietly change meaning: which services exist, the image each
/// one runs, and the exact text of every label and environment value
/// after Compose's own scalar handling. Those are the values the issue
/// names as the likely first bugs — Traefik's `Host(...)` rules carry
/// backticks, `expose` entries are bare numbers, and Compose coerces
/// scalars on the way in.
fn compare(name: &str, input: &serde_yaml_ng::Value, output: &serde_yaml_ng::Value) -> Vec<String> {
    let mut failures = Vec::new();
    let in_services = services_of(input);
    let out_services = services_of(output);

    let in_names: BTreeSet<&str> = in_services.keys().copied().collect();
    let out_names: BTreeSet<&str> = out_services.keys().copied().collect();
    if in_names != out_names {
        failures.push(format!(
            "{name}: Compose read a different set of services: generated {in_names:?}, \
             Compose reported {out_names:?}"
        ));
        return failures;
    }

    for (service, generated) in &in_services {
        let read = &out_services[service];
        let at = format!("{name}: service `{service}`");

        let in_image = generated.get("image").and_then(|v| v.as_str());
        let out_image = read.get("image").and_then(|v| v.as_str());
        if in_image != out_image {
            failures.push(format!(
                "{at}: image changed: generated {in_image:?}, Compose read {out_image:?}"
            ));
        }

        // `expose` survives as a list either way, but Compose stringifies
        // each entry, so the comparison is over rendered scalars.
        let in_expose = scalar_list(generated.get("expose"));
        let out_expose = scalar_list(read.get("expose"));
        if in_expose != out_expose {
            failures.push(format!(
                "{at}: expose changed: generated {in_expose:?}, Compose read {out_expose:?}"
            ));
        }

        // `labels` and `environment` are emitted as `key=value` lists and
        // come back as maps, so each generated entry is looked up by key.
        for field in ["labels", "environment"] {
            for (key, value) in pairs(generated.get(field)) {
                match read.get(field).and_then(|m| m.get(key.as_str())) {
                    None => failures.push(format!(
                        "{at}: {field} entry `{key}` is missing from Compose's own reading"
                    )),
                    Some(read_value) => {
                        let read_text = scalar(read_value);
                        if read_text.as_deref() != Some(value.as_str()) {
                            failures.push(format!(
                                "{at}: {field} `{key}` changed: generated {value:?}, \
                                 Compose read {read_text:?}"
                            ));
                        }
                    }
                }
            }
        }
    }

    failures
}

fn services_of(doc: &serde_yaml_ng::Value) -> BTreeMap<&str, &serde_yaml_ng::Value> {
    doc.get("services")
        .and_then(|s| s.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| k.as_str().map(|k| (k, v)))
                .collect()
        })
        .unwrap_or_default()
}

/// A YAML scalar as the text Compose would compare, so `8384` and
/// `"8384"` are the same value here — Compose itself treats them that
/// way for `expose` and for label values.
fn scalar(value: &serde_yaml_ng::Value) -> Option<String> {
    match value {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn scalar_list(value: Option<&serde_yaml_ng::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(scalar).collect())
        .unwrap_or_default()
}

/// Splits a `key=value` list — codegen's shape for `labels` and
/// `environment` — at the first `=`, which is where Compose splits it
/// too. An entry with no `=` has no value to compare and is dropped.
fn pairs(value: Option<&serde_yaml_ng::Value>) -> Vec<(String, String)> {
    value
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(scalar)
                .filter_map(|entry| {
                    entry
                        .split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(corpus: &str, cases: Vec<Case>) {
    require_docker_compose();
    let dir = scratch_dir(corpus);

    let mut failures = Vec::new();
    for case in &cases {
        failures.extend(check(case, &dir));
    }
    fs::remove_dir_all(&dir).ok();

    assert!(
        failures.is_empty(),
        "\n{} of {} {corpus} document(s) disagree with Docker Compose:\n\n{}\n",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}

#[test]
fn parser_fixtures_are_valid_compose() {
    run("fixtures", parser_fixture_cases());
}

#[test]
fn book_build_examples_are_valid_compose() {
    run("book", book_build_cases());
}
