//! One integration test proving `FsLoader` (the real-filesystem
//! `FileLoader`) works end-to-end, not just `InMemoryLoader` — the rest
//! of this crate's tests (`link_tests.rs`) use `InMemoryLoader` so they
//! don't need real files on disk.

use std::fs;
use std::path::PathBuf;

use hl_linker::{FsLoader, link};

/// A scratch directory under the target dir, unique per test process —
/// same precedent as `hl-cli/tests/build_tests.rs`'s own helper, to
/// avoid a `tempfile` dev-dependency for a one-off need.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hl-linker-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn links_a_real_multi_file_tree_on_disk() {
    let dir = scratch_dir("fs-loader");
    fs::write(
        dir.join("docker.hll"),
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("service.hll"),
        "use \"docker.hll\" as traefik\n\
         service jellyfin {\n  image \"jellyfin/jellyfin\"\n  networks [traefik.traefik-net]\n}\n",
    )
    .unwrap();

    let composed = link(&dir.join("service.hll"), &FsLoader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

    assert_eq!(composed.services.len(), 1);
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default"
    );

    fs::remove_dir_all(&dir).ok();
}
