//! Integration tests for [`hl_linker::link`] over [`InMemoryLoader`] —
//! proves the four import rules from docs/DESIGN.md against a real,
//! multi-file `use` graph (as opposed to Stage 1's `hl-parser`
//! `multi_scope_tests.rs`, which proved the same scoping contract with a
//! hand-rolled in-memory `SymbolResolver` and no real graph-loading at
//! all).

use std::path::Path;

use hl_linker::{InMemoryLoader, LinkError, link};
use hl_parser::ComposeError;

fn image_of(composed: &hl_parser::ComposedProgram, index: usize) -> &str {
    composed.services[index]
        .fields
        .image
        .as_ref()
        .unwrap()
        .reference
        .as_ref()
        .unwrap()
        .text()
}

#[test]
fn two_hop_template_and_network_resolution() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "docker.hll",
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n",
    );
    loader.add(
        "service.hll",
        "use \"docker.hll\" as traefik\n\
         service jellyfin {\n  image \"jellyfin/jellyfin\"\n  networks [traefik.traefik-net]\n}\n",
    );

    let composed = link(Path::new("service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

    assert_eq!(image_of(&composed, 0), "jellyfin/jellyfin");
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default"
    );
    let network_ref = &composed.services[0].fields.networks[0];
    assert!(network_ref.qualifier.is_none());
    assert_eq!(network_ref.name, "traefik-net");
}

#[test]
fn three_hop_template_reuse_through_an_imported_templates_file() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "docker.hll",
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n",
    );
    loader.add(
        "templates.hll",
        "use \"docker.hll\" as traefik\n\
         template web {\n  networks [traefik.traefik-net]\n  restart unless-stopped\n}\n",
    );
    loader.add(
        "service.hll",
        "use \"templates.hll\" as common\n\
         service jellyfin {\n  with common.web\n  image \"jellyfin/jellyfin\"\n}\n",
    );

    let composed = link(Path::new("service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

    assert_eq!(image_of(&composed, 0), "jellyfin/jellyfin");
    assert_eq!(
        composed.services[0]
            .fields
            .restart
            .as_ref()
            .unwrap()
            .policy
            .as_ref()
            .unwrap()
            .text(),
        "unless-stopped"
    );
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default"
    );
}

#[test]
fn template_qualified_reference_resolves_in_its_own_declaring_file_not_the_invokers() {
    // Two networks named `traefik-net`, distinguished by `real_name`.
    // `service.hll`'s own `traefik` alias points at the decoy;
    // `templates.hll`'s own `traefik` alias (declared in *its* `use`)
    // points at the real one. The real one must win.
    let mut loader = InMemoryLoader::default();
    loader.add(
        "docker.hll",
        "network traefik-net {\n  name: \"docker_default\"\n}\n",
    );
    loader.add(
        "decoy.hll",
        "network traefik-net {\n  name: \"decoy_network\"\n}\n",
    );
    loader.add(
        "templates.hll",
        "use \"docker.hll\" as traefik\n\
         template web {\n  networks [traefik.traefik-net]\n}\n",
    );
    loader.add(
        "service.hll",
        "use \"templates.hll\" as common\n\
         use \"decoy.hll\" as traefik\n\
         service s {\n  with common.web\n}\n",
    );

    let composed = link(Path::new("service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default",
        "expected templates.hll's own `traefik` alias (Docker) to win, not service.hll's (Decoy)"
    );
}

#[test]
fn imports_are_not_transitive() {
    let mut loader = InMemoryLoader::default();
    loader.add("docker.hll", "network traefik-net {\n  external\n}\n");
    loader.add(
        "templates.hll",
        "use \"docker.hll\" as traefik\n\
         template web {\n  networks [traefik.traefik-net]\n}\n",
    );
    loader.add(
        "service.hll",
        // `service.hll` only `use`s `templates.hll` — it never `use`s
        // `docker.hll` itself, so its own body can't write `traefik.*`
        // even though `templates.hll` (which it does use) can.
        "use \"templates.hll\" as common\n\
         service s {\n  with common.web\n  networks [traefik.traefik-net]\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::UnknownAlias { alias, .. }) if alias == "traefik"
    ));
}

#[test]
fn cyclic_use_graph_composes_successfully() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "a.hll",
        "use \"b.hll\" as b\n\
         template ta {\n  with b.tb\n  image \"from-a\"\n}\n\
         service s {\n  with ta\n}\n",
    );
    loader.add(
        "b.hll",
        "use \"a.hll\" as a\n\
         template tb {\n  image \"from-b\"\n}\n",
    );

    let composed = link(Path::new("a.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error (does the graph loader hang or false-error on a cyclic use graph?): {err}"));

    assert_eq!(image_of(&composed, 0), "from-a");
}

#[test]
fn relative_paths_resolve_against_each_importing_files_own_directory() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "shared/docker.hll",
        "network traefik-net {\n  name: \"docker_default\"\n}\n",
    );
    loader.add(
        "shared/templates.hll",
        "use \"docker.hll\" as traefik\n\
         template web {\n  networks [traefik.traefik-net]\n}\n",
    );
    loader.add(
        "services/foo/service.hll",
        "use \"../../shared/templates.hll\" as common\n\
         service s {\n  with common.web\n}\n",
    );

    let composed = link(Path::new("services/foo/service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default"
    );
}

#[test]
fn missing_file_is_io_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "service.hll",
        "use \"missing.hll\" as x\nservice s {\n  image \"x\"\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Io { path, .. } if path == Path::new("missing.hll")
    ));
}

#[test]
fn duplicate_alias_in_one_file_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "service.hll",
        "use \"a.hll\" as x\nuse \"b.hll\" as x\nservice s {\n  image \"x\"\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::DuplicateAlias { alias, .. } if alias == "x"
    ));
}

#[test]
fn unknown_qualified_network_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add("docker.hll", "network traefik-net {\n  external\n}\n");
    loader.add(
        "service.hll",
        "use \"docker.hll\" as traefik\n\
         service s {\n  image \"x\"\n  networks [traefik.nonexistent]\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::UnknownQualifiedNetwork { alias, name, .. })
            if alias == "traefik" && name == "nonexistent"
    ));
}

/// The template-side counterpart to `unknown_qualified_network_is_error`:
/// the alias resolves to a real imported module, but that module has no
/// template by the invoked name.
#[test]
fn unknown_qualified_template_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add("templates.hll", "template web {\n  image \"x\"\n}\n");
    loader.add(
        "service.hll",
        "use \"templates.hll\" as common\n\
         service s {\n  with common.nonexistent\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::UnknownTemplate { name, .. })
            if name == "nonexistent"
    ));
}
