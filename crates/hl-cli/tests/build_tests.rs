//! Integration tests for `hl-cli`'s `--build` wiring: parse → compose →
//! generate → write to disk. The underlying pipeline stages have their
//! own extensive test coverage (`hl-parser`, `hl-codegen`); these tests
//! only check the CLI's own plumbing — argument handling, file I/O,
//! directory iteration.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use hl_cli::{Cli, run};

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
    });
    assert_eq!(code, ExitCode::SUCCESS);

    fs::remove_dir_all(&dir).ok();
}
