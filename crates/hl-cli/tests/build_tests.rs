//! Integration tests for `hl-cli`'s `--build` wiring: parse → compose →
//! generate → write to disk. The underlying pipeline stages have their
//! own extensive test coverage (`hl-parser`, `hl-codegen`); these tests
//! only check the CLI's own plumbing — argument handling, file I/O,
//! directory iteration.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use hl_cli::{Cli, GENERATED_HEADER, run};

const HAND_WRITTEN_COMPOSE: &str = "\
# hand-written by me, tuned over months
services:
  s:
    image: nginx
    deploy: { resources: { limits: { memory: 512M } } }
";

const SYNCTHING_HLL: &str = include_str!("../../hl-parser/tests/fixtures/syncthing.hll");

const IMPORTS_NETWORK_HLL: &str = include_str!("fixtures/imports/network.hll");
const IMPORTS_TEMPLATES_HLL: &str = include_str!("fixtures/imports/templates.hll");
const IMPORTS_SYNCTHING_HLL: &str = include_str!("fixtures/imports/syncthing.hll");

/// A scratch directory under the target dir, unique per test process, so
/// parallel test runs don't collide — avoids adding a `tempfile`
/// dev-dependency for what's otherwise a one-off need.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hl-cli-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the real `hllc` binary (built by Cargo before integration tests
/// run, per `CARGO_BIN_EXE_<name>`) as a subprocess and returns its
/// captured output. `run()`'s own `eprintln!`/`println!` calls write
/// straight to the process's real stdout/stderr, which an in-process
/// call to `run()` has no way to intercept — the zero-output/skip
/// messages this file's tests check below only exist as printed text,
/// with no other observable side effect, so this is the one place that
/// needs the real binary rather than calling `run()` directly.
fn hllc_output(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_hllc"))
        .args(args)
        .output()
        .expect("failed to run the hllc binary")
}

#[test]
fn build_single_file_writes_yaml_to_out_path() {
    let dir = scratch_dir("single-file");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let yaml = fs::read_to_string(&out).unwrap();
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_single_file_creates_missing_out_parent_directories() {
    let dir = scratch_dir("single-file-nested-out");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("nested").join("deeper").join("docker-compose.yml");
    assert!(!out.parent().unwrap().exists());

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let yaml = fs::read_to_string(&out).unwrap();
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_directory_writes_one_file_per_hll_input() {
    let dir = scratch_dir("directory");
    fs::write(dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = out_dir.join("syncthing").join("docker-compose.yml");
    assert!(
        generated.exists(),
        "expected {} to exist",
        generated.display()
    );
    let yaml = fs::read_to_string(&generated).unwrap();
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

/// `crates/hl-cli/tests/fixtures/imports/` splits the same service
/// `SYNCTHING_HLL` declares into `network.hll` + `templates.hll` +
/// `syncthing.hll`, connected by real `use` decls. Building the split
/// version through a real on-disk `use` graph (not `hl-linker`'s own
/// `InMemoryLoader`-backed unit tests) should produce byte-identical
/// output to building the original one-file version.
#[test]
fn build_directory_of_hll_files_with_use_imports_between_them() {
    let dir = scratch_dir("imports");
    fs::write(dir.join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();
    fs::write(dir.join("templates.hll"), IMPORTS_TEMPLATES_HLL).unwrap();
    let input = dir.join("syncthing.hll");
    fs::write(&input, IMPORTS_SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let imported_yaml = fs::read_to_string(&out).unwrap();

    let single_file_dir = scratch_dir("imports-baseline");
    let single_file_input = single_file_dir.join("syncthing.hll");
    fs::write(&single_file_input, SYNCTHING_HLL).unwrap();
    let single_file_out = single_file_dir.join("docker-compose.yml");
    let code = run(Cli {
        file: single_file_input,
        parse: false,
        build: true,
        out: Some(single_file_out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);
    let baseline_yaml = fs::read_to_string(&single_file_out).unwrap();

    assert_eq!(
        imported_yaml, baseline_yaml,
        "splitting syncthing.hll across network.hll/templates.hll/syncthing.hll \
         should produce identical Compose output to the original one-file version"
    );

    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&single_file_dir).ok();
}

#[test]
fn build_directory_without_out_is_error() {
    let dir = scratch_dir("no-out");
    fs::write(dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);

    fs::remove_dir_all(&dir).ok();
}

/// #12: a root directory whose immediate children are themselves
/// directories (not `.hll` files) — the real homelab-infrastructure
/// layout, `<service>/<service>.hll` next to `<service>/docker-compose.yml`
/// — builds each child's `.hll` file in place, with no `--out` needed.
#[test]
fn build_colocated_service_directories_writes_in_place_with_no_out() {
    let dir = scratch_dir("colocated");
    let syncthing_dir = dir.join("syncthing");
    fs::create_dir_all(&syncthing_dir).unwrap();
    fs::write(syncthing_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = syncthing_dir.join("docker-compose.yml");
    assert!(
        generated.exists(),
        "expected {} to exist",
        generated.display()
    );
    let yaml = fs::read_to_string(&generated).unwrap();
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

/// Same co-located layout, but with `--out` given — remaps the whole
/// tree the same way flat mode's `--out` already does, just keyed by
/// each subdirectory's own name.
#[test]
fn build_colocated_service_directories_with_out_remaps_tree() {
    let dir = scratch_dir("colocated-out");
    let syncthing_dir = dir.join("syncthing");
    fs::create_dir_all(&syncthing_dir).unwrap();
    fs::write(syncthing_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = out_dir.join("syncthing").join("docker-compose.yml");
    assert!(
        generated.exists(),
        "expected {} to exist",
        generated.display()
    );
    assert!(
        !syncthing_dir.join("docker-compose.yml").exists(),
        "with --out given, output should go there instead of in place"
    );

    fs::remove_dir_all(&dir).ok();
}

/// A co-located service directory with more than one `.hll` file is
/// ambiguous (which one's output is "that same directory"'s
/// `docker-compose.yml`?) — an explicit error, not a silent guess.
#[test]
fn build_colocated_directory_with_multiple_hll_files_is_error() {
    let dir = scratch_dir("colocated-ambiguous");
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    fs::write(service_dir.join("extra.hll"), SYNCTHING_HLL).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);

    fs::remove_dir_all(&dir).ok();
}

/// A root directory with neither `.hll` files nor any subdirectory
/// containing them builds nothing and still succeeds — matching the
/// existing flat case's behavior when it finds zero `.hll` files.
#[test]
fn build_directory_with_no_hll_files_anywhere_is_a_no_op_success() {
    let dir = scratch_dir("empty");
    fs::create_dir_all(dir.join("not-a-service")).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    fs::remove_dir_all(&dir).ok();
}

/// #89: the exact layout `book/src/getting-started.md` teaches —
/// `homelab/shared/{network.hll,templates.hll}` (a library directory,
/// declaring no service) alongside `homelab/services/<service>/*.hll`
/// (real service directories one level *further* than the old
/// one-level-only scan ever looked). Before this fix, `shared/`'s two
/// `.hll` files hard-errored the whole build ("expected exactly one per
/// co-located service directory"), and removing `shared/` still built
/// nothing: `services/` itself holds no `.hll` files, so the old scan
/// silently skipped it and exited 0.
#[test]
fn build_book_layout_with_library_directory_and_nested_services() {
    let dir = scratch_dir("book-layout");
    let shared_dir = dir.join("shared");
    fs::create_dir_all(&shared_dir).unwrap();
    fs::write(shared_dir.join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();
    fs::write(shared_dir.join("templates.hll"), IMPORTS_TEMPLATES_HLL).unwrap();

    let services_dir = dir.join("services");
    let jellyfin_dir = services_dir.join("jellyfin");
    fs::create_dir_all(&jellyfin_dir).unwrap();
    fs::write(jellyfin_dir.join("jellyfin.hll"), SYNCTHING_HLL).unwrap();
    let syncthing_dir = services_dir.join("syncthing");
    fs::create_dir_all(&syncthing_dir).unwrap();
    fs::write(syncthing_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    for service_dir in [&jellyfin_dir, &syncthing_dir] {
        let generated = service_dir.join("docker-compose.yml");
        assert!(
            generated.exists(),
            "expected {} to exist",
            generated.display()
        );
    }
    assert!(
        !shared_dir.join("docker-compose.yml").exists(),
        "shared/ declares no service and shouldn't produce output"
    );
    assert!(!services_dir.join("docker-compose.yml").exists());

    fs::remove_dir_all(&dir).ok();
}

/// Same layout, but with `--out` given: each service directory's output
/// lands under `<out>/<path-from-root>/`, preserving the tree's own
/// relative structure rather than flattening it — the co-located
/// equivalent of flat mode's per-file-stem remapping.
#[test]
fn build_book_layout_with_out_preserves_relative_structure() {
    let dir = scratch_dir("book-layout-out");
    let services_dir = dir.join("services");
    let jellyfin_dir = services_dir.join("jellyfin");
    fs::create_dir_all(&jellyfin_dir).unwrap();
    fs::write(jellyfin_dir.join("jellyfin.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = out_dir
        .join("services")
        .join("jellyfin")
        .join("docker-compose.yml");
    assert!(
        generated.exists(),
        "expected {} to exist",
        generated.display()
    );
    assert!(!jellyfin_dir.join("docker-compose.yml").exists());

    fs::remove_dir_all(&dir).ok();
}

/// #89: pointing `--build` (flat mode, since `shared/` holds `.hll`
/// files directly) at a library directory used to produce one useless
/// `docker-compose.yml` per file (`services: {}`, every declaration
/// dropped). Both files here declare no service and should be skipped
/// entirely, leaving the build a documented no-op rather than junk
/// output a user might `docker compose up`.
#[test]
fn build_flat_directory_skips_library_only_files() {
    let dir = scratch_dir("flat-library-only");
    fs::write(dir.join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();
    fs::write(dir.join("templates.hll"), IMPORTS_TEMPLATES_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        !out_dir.exists() || fs::read_dir(&out_dir).unwrap().next().is_none(),
        "expected no output to be written for a directory of library-only files"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The flip side: a flat directory mixing one real service file with
/// library-only files builds only the service, skipping the rest,
/// rather than failing or producing junk for the others.
#[test]
fn build_flat_directory_builds_only_the_files_that_declare_a_service() {
    let dir = scratch_dir("flat-mixed");
    fs::write(dir.join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();
    fs::write(dir.join("templates.hll"), IMPORTS_TEMPLATES_HLL).unwrap();
    fs::write(dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = out_dir.join("syncthing").join("docker-compose.yml");
    assert!(generated.exists());
    assert!(!out_dir.join("network").exists());
    assert!(!out_dir.join("templates").exists());

    fs::remove_dir_all(&dir).ok();
}

/// The "nothing was built" message (#89) is printed exactly when the
/// running `built` count stayed zero — checked from both directions, so
/// a `==`/`!=` swap on that check, or a `+=`/`*=` swap on the counter
/// itself (which would leave `built` stuck at zero even after a real
/// build), each flips one of these two assertions.
#[test]
fn build_flat_directory_prints_no_message_when_something_builds() {
    let dir = scratch_dir("flat-message-positive");
    fs::write(dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let output = hllc_output(&[
        "--build",
        dir.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("nothing was built"),
        "expected no 'nothing was built' message when a service did build, got: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_flat_directory_prints_a_message_when_nothing_builds() {
    let dir = scratch_dir("flat-message-negative");
    fs::write(dir.join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();
    let out_dir = dir.join("out");

    let output = hllc_output(&[
        "--build",
        dir.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing was built"),
        "expected a 'nothing was built' message when no service built, got: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// Same pair, for co-located mode's own `built` counter and zero-output
/// check in `run_build`.
#[test]
fn build_colocated_prints_no_message_when_something_builds() {
    let dir = scratch_dir("colocated-message-positive");
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();

    let output = hllc_output(&["--build", dir.to_str().unwrap()]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("nothing was built"),
        "expected no 'nothing was built' message when a service did build, got: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_colocated_prints_a_message_when_nothing_builds() {
    let dir = scratch_dir("colocated-message-negative");
    fs::create_dir_all(dir.join("shared")).unwrap();
    fs::write(dir.join("shared").join("network.hll"), IMPORTS_NETWORK_HLL).unwrap();

    let output = hllc_output(&["--build", dir.to_str().unwrap()]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing was built"),
        "expected a 'nothing was built' message when no service built, got: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// #89: without the recursion-depth cap, a symlinked directory pointing
/// back at one of its own ancestors turns the co-located scan into
/// unbounded recursion — this creates exactly that cycle and confirms
/// the build fails cleanly (rather than hanging or overflowing the
/// stack) once the cap is hit. Also the only test that actually exercises
/// the depth counter incrementing on each recursive step, rather than a
/// real (finite, small) tree that would terminate on its own regardless
/// of whether the counter worked. Unix-only: no portable way to create a
/// symlink.
#[cfg(unix)]
#[test]
fn build_colocated_directory_cycle_is_stopped_by_the_depth_cap() {
    let dir = scratch_dir("colocated-symlink-cycle");
    let inner = dir.join("inner");
    fs::create_dir_all(&inner).unwrap();
    std::os::unix::fs::symlink(&dir, inner.join("loop")).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(
        code,
        ExitCode::FAILURE,
        "expected the recursion depth cap to stop a symlink cycle rather than hang or crash"
    );

    fs::remove_dir_all(&dir).ok();
}

/// #64: `fs::write` follows symlinks, so a `docker-compose.yml` replaced
/// by a link to an unrelated file used to have that file truncated and
/// overwritten by a plain `hllc --build <repo>`. Unix-only: there's no
/// portable way to create a symlink, and the guard itself is
/// platform-independent.
#[cfg(unix)]
#[test]
fn build_refuses_to_write_through_a_symlinked_compose_file() {
    let dir = scratch_dir("symlink-colocated");
    let victim = dir.join("victim.txt");
    fs::write(&victim, "precious\n").unwrap();
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    std::os::unix::fs::symlink(&victim, service_dir.join("docker-compose.yml")).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "precious\n",
        "the symlink's target must not have been written through"
    );

    fs::remove_dir_all(&dir).ok();
}

/// The same guard on the `--out` path, which names its destination
/// directly rather than deriving it from a directory scan.
#[cfg(unix)]
#[test]
fn build_refuses_a_symlinked_out_path() {
    let dir = scratch_dir("symlink-out");
    let victim = dir.join("victim.txt");
    fs::write(&victim, "precious\n").unwrap();
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");
    std::os::unix::fs::symlink(&victim, &out).unwrap();

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out),
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "precious\n");

    fs::remove_dir_all(&dir).ok();
}

/// #64: `Path::new("...hll").file_stem()` is `..`, so joining the stem
/// onto `--out` used to escape one level above the chosen output
/// directory.
#[test]
fn build_rejects_a_parent_escaping_file_stem() {
    let dir = scratch_dir("escaping-stem");
    let inputs = dir.join("inputs");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(inputs.join("...hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");

    let code = run(Cli {
        file: inputs,
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        !dir.join("docker-compose.yml").exists(),
        "output must not have escaped above the --out directory"
    );

    fs::remove_dir_all(&dir).ok();
}

/// #64, the sibling case: `Path::new("..hll").file_stem()` is `.`, which
/// doesn't escape `--out` but collapses *into* it — every such file
/// would write to `<out>/docker-compose.yml`, silently overwriting each
/// other rather than landing in one subdirectory per input.
#[test]
fn build_rejects_a_current_directory_file_stem() {
    let dir = scratch_dir("current-dir-stem");
    let inputs = dir.join("inputs");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(inputs.join("..hll"), SYNCTHING_HLL).unwrap();
    // Created up front deliberately: `create_dir_all("out/.")` fails
    // outright when `out` doesn't exist yet, so without this the build
    // would fail on that IO error instead of on the guard, and the test
    // would pass whether or not the guard is doing anything.
    let out_dir = dir.join("out");
    fs::create_dir_all(&out_dir).unwrap();

    let code = run(Cli {
        file: inputs,
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        !out_dir.join("docker-compose.yml").exists(),
        "output must not have collapsed into the --out directory itself"
    );

    fs::remove_dir_all(&dir).ok();
}

/// #85: every `--build` output carries a header identifying it as
/// generated — that's what makes generated files tellable apart from
/// authored ones in a repo, and what the overwrite guard below keys off.
/// The header is a YAML comment, so the document still parses.
#[test]
fn build_output_starts_with_the_generated_header() {
    let dir = scratch_dir("generated-header");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let yaml = fs::read_to_string(&out).unwrap();
    assert!(
        yaml.starts_with(GENERATED_HEADER),
        "output should start with the generated header, got:\n{yaml}"
    );
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should still be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

/// #85, the core case: co-located mode discovers `docker-compose.yml` by
/// scanning, so a hand-written one — the rest of a repo mid-migration —
/// used to be destroyed without a word. It must survive untouched, and
/// the build must fail rather than half-succeed silently.
#[test]
fn build_refuses_to_overwrite_a_hand_written_compose_file() {
    let dir = scratch_dir("hand-written-colocated");
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let compose = service_dir.join("docker-compose.yml");
    fs::write(&compose, HAND_WRITTEN_COMPOSE).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(
        fs::read_to_string(&compose).unwrap(),
        HAND_WRITTEN_COMPOSE,
        "the hand-written file must not have been touched"
    );

    fs::remove_dir_all(&dir).ok();
}

/// `--force` is the documented way past that guard, for the user who
/// really does want the hand-written file replaced.
#[test]
fn build_force_overwrites_a_hand_written_compose_file() {
    let dir = scratch_dir("hand-written-force");
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let compose = service_dir.join("docker-compose.yml");
    fs::write(&compose, HAND_WRITTEN_COMPOSE).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: None,
        force: true,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let yaml = fs::read_to_string(&compose).unwrap();
    assert!(yaml.starts_with(GENERATED_HEADER));
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("output should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

/// The guard must not make rebuilding require `--force`: `hllc`'s own
/// previous output carries the header, so a second plain build over it
/// succeeds — the everyday case.
#[test]
fn build_overwrites_its_own_previous_output_without_force() {
    let dir = scratch_dir("rebuild");
    let service_dir = dir.join("syncthing");
    fs::create_dir_all(&service_dir).unwrap();
    fs::write(service_dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();

    for _ in 0..2 {
        let code = run(Cli {
            file: dir.clone(),
            parse: false,
            build: true,
            out: None,
            force: false,
        });
        assert_eq!(code, ExitCode::SUCCESS);
    }

    let yaml = fs::read_to_string(service_dir.join("docker-compose.yml")).unwrap();
    assert!(yaml.starts_with(GENERATED_HEADER));
    assert_eq!(
        yaml.matches(GENERATED_HEADER).count(),
        1,
        "rebuilding should replace the file, not accumulate headers"
    );

    fs::remove_dir_all(&dir).ok();
}

/// #85 asks for the guard on all three write paths, not just the
/// co-located one. Flat mode derives `<out>/<stem>/docker-compose.yml`
/// from a scan too, so a pre-existing hand-written file can be sitting
/// there just the same.
#[test]
fn build_flat_mode_refuses_to_overwrite_a_hand_written_compose_file() {
    let dir = scratch_dir("hand-written-flat");
    fs::write(dir.join("syncthing.hll"), SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("out");
    let existing_dir = out_dir.join("syncthing");
    fs::create_dir_all(&existing_dir).unwrap();
    let compose = existing_dir.join("docker-compose.yml");
    fs::write(&compose, HAND_WRITTEN_COMPOSE).unwrap();

    let code = run(Cli {
        file: dir.clone(),
        parse: false,
        build: true,
        out: Some(out_dir),
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(fs::read_to_string(&compose).unwrap(), HAND_WRITTEN_COMPOSE);

    fs::remove_dir_all(&dir).ok();
}

/// And on the `--out` path, where the destination is named directly.
#[test]
fn build_refuses_a_hand_written_out_path() {
    let dir = scratch_dir("hand-written-out");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");
    fs::write(&out, HAND_WRITTEN_COMPOSE).unwrap();

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(fs::read_to_string(&out).unwrap(), HAND_WRITTEN_COMPOSE);

    fs::remove_dir_all(&dir).ok();
}

/// #88: an existing *directory* named as `--out` in single-file mode
/// writes `docker-compose.yml` inside it, matching directory mode's own
/// convention — `dist/` is the natural first guess for `--out` (every
/// directory-build example in the book uses exactly this shape), and it
/// previously failed with a bare `Is a directory (os error 21)`.
#[test]
fn build_single_file_writes_into_an_existing_out_directory() {
    let dir = scratch_dir("out-is-existing-dir");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out_dir = dir.join("dist");
    fs::create_dir_all(&out_dir).unwrap();

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out_dir.clone()),
        force: false,
    });
    assert_eq!(code, ExitCode::SUCCESS);

    let generated = out_dir.join("docker-compose.yml");
    assert!(
        generated.exists(),
        "expected {} to exist",
        generated.display()
    );

    fs::remove_dir_all(&dir).ok();
}

/// A path that exists but can't be read back for inspection — here a
/// permission-denied file — is refused rather than assumed disposable:
/// nothing about it could be shown to be generated output. Unix-only:
/// there's no portable way to make a file unreadable to its own owner.
#[cfg(unix)]
#[test]
fn build_refuses_an_out_path_it_cannot_inspect() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("uninspectable-out");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");
    fs::write(&out, HAND_WRITTEN_COMPOSE).unwrap();
    fs::set_permissions(&out, fs::Permissions::from_mode(0o000)).unwrap();

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out.clone()),
        force: false,
    });

    fs::set_permissions(&out, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        HAND_WRITTEN_COMPOSE,
        "the existing file must be left alone"
    );

    fs::remove_dir_all(&dir).ok();
}

/// `--force` waives the #85 header check, not the #64 symlink refusal:
/// a link's problem is that the write lands somewhere other than the
/// path in hand, which isn't what `--force` asks for. Pinned separately
/// because with `--force` the header check is out of the picture, so
/// only the symlink guard can keep the victim intact.
#[cfg(unix)]
#[test]
fn build_force_still_refuses_to_write_through_a_symlink() {
    let dir = scratch_dir("symlink-force");
    let victim = dir.join("victim.txt");
    fs::write(&victim, "precious\n").unwrap();
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();
    let out = dir.join("docker-compose.yml");
    std::os::unix::fs::symlink(&victim, &out).unwrap();

    let code = run(Cli {
        file: input,
        parse: false,
        build: true,
        out: Some(out),
        force: true,
    });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "precious\n");

    fs::remove_dir_all(&dir).ok();
}

/// #88: piping `--parse`'s AST dump into something that closes its end
/// early (`head`, `less`, ...) — the only practical way to read what can
/// be a many-thousand-line `{:#?}` dump — used to hand the user a Rust
/// panic and backtrace the moment the reader closed the pipe, since
/// `println!` panics on any stdout write error including a closed pipe.
/// Reproduces that exact shape: closes the read end before the child
/// (spawned as the real binary, since the panic lives in `println!`'s
/// own machinery, not anything `run()` returns a `Result` for) has
/// finished writing, and asserts a clean exit rather than a crash.
#[test]
fn parse_does_not_panic_when_stdout_closes_early() {
    let dir = scratch_dir("epipe");
    let input = dir.join("big.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hllc"))
        .args(["--parse", input.to_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn hllc");

    // Close the read end without reading anything, simulating `| head`
    // closing its end as soon as it has what it wants — any write the
    // child makes after this point observes a closed pipe.
    drop(child.stdout.take());

    let status = child.wait().expect("failed to wait on hllc");
    assert!(
        status.success(),
        "expected a clean exit on a closed pipe, got {status:?}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// `--build` with no `--out` prints the generated YAML through
/// `write_stdout` to the process's real stdout. `run()` called in-process
/// can't observe that (see `hllc_output`'s doc comment), so this has to
/// go through the real binary — it's the only thing that would notice
/// `write_stdout` silently doing nothing instead of writing.
#[test]
fn build_single_file_with_no_out_prints_yaml_to_real_stdout() {
    let dir = scratch_dir("stdout-yaml");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();

    let output = hllc_output(&["--build", input.to_str().unwrap()]);
    assert!(output.status.success());

    let yaml = String::from_utf8(output.stdout).unwrap();
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("stdout should be valid YAML");
    assert!(
        value
            .get("services")
            .and_then(|s| s.get("syncthing"))
            .is_some()
    );

    fs::remove_dir_all(&dir).ok();
}

/// `write_stdout` treats a closed pipe (`BrokenPipe`) as a clean exit,
/// but any *other* write failure is a real error and must still fail
/// loudly. `/dev/full` always fails writes with `ENOSPC`, a distinct
/// `io::ErrorKind` from `BrokenPipe` — the one write error that's both
/// reliably reproducible and clearly not a closed reader, so this is
/// what actually exercises the non-`BrokenPipe` arm of `write_stdout`'s
/// match.
#[test]
#[cfg(unix)]
fn build_reports_a_real_stdout_write_failure_instead_of_exiting_clean() {
    let dir = scratch_dir("stdout-write-failure");
    let input = dir.join("syncthing.hll");
    fs::write(&input, SYNCTHING_HLL).unwrap();

    let dev_full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("this test requires /dev/full");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hllc"))
        .args(["--build", input.to_str().unwrap()])
        .stdout(dev_full)
        .output()
        .expect("failed to run the hllc binary");

    assert!(
        !output.status.success(),
        "a real stdout write failure should exit non-zero, got {:?}",
        output.status
    );
    assert!(
        !output.stderr.is_empty(),
        "a real stdout write failure should print a diagnostic to stderr"
    );

    fs::remove_dir_all(&dir).ok();
}
