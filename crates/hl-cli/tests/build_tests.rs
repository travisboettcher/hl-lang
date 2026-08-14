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
