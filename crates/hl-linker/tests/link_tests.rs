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
fn implicit_defaults_template_applies_through_the_module_graph() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "service.hll",
        "template defaults {\n  restart unless-stopped\n}\n\
         service s {\n  image \"x\"\n}\n",
    );

    let composed = link(Path::new("service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));

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
        "unless-stopped",
        "the module's own `defaults` template should apply even though `s` never `with`s it explicitly"
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

/// #63, linker side: the graph's own declaration collection rejects a
/// redeclared service, same as `hl_parser::compose` does for a lone
/// file.
#[test]
fn duplicate_service_name_in_one_file_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "service.hll",
        "service web {\n  image \"a\"\n}\nservice web {\n  image \"b\"\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::DuplicateServiceName { name, .. }) if name == "web"
    ));
}

/// A duplicate `service` in an *imported* file is inert (only the entry
/// module's services are built), but it's still a redeclaration, so it's
/// still reported — mirroring how a duplicate template is rejected in
/// every module, entry or not.
#[test]
fn duplicate_service_name_in_an_imported_file_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "lib.hll",
        "service web {\n  image \"a\"\n}\nservice web {\n  image \"b\"\n}\n",
    );
    loader.add(
        "service.hll",
        "use \"lib.hll\" as lib\nservice s {\n  image \"x\"\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::DuplicateServiceName { name, .. }) if name == "web"
    ));
}

/// #63: without this check the entry module's `entry_networks` (source
/// order, first-wins downstream) and its by-name `networks` map
/// (last-wins) disagree about which `proxy` a reference means.
#[test]
fn duplicate_network_name_in_one_file_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "service.hll",
        "network proxy {\n  external\n}\nnetwork proxy {\n  name: \"other\"\n}\n\
         service s {\n  image \"x\"\n  networks [proxy]\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::DuplicateNetworkName { name, .. }) if name == "proxy"
    ));
}

/// The imported-file case of the same check: a library module's
/// `networks` map is what every `alias.name` lookup goes through, so a
/// duplicate there is exactly the ambiguity this rejects.
#[test]
fn duplicate_network_name_in_an_imported_file_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "docker.hll",
        "network proxy {\n  external\n}\nnetwork proxy {\n  name: \"other\"\n}\n",
    );
    loader.add(
        "service.hll",
        "use \"docker.hll\" as traefik\n\
         service s {\n  image \"x\"\n  networks [traefik.proxy]\n}\n",
    );

    let err = link(Path::new("service.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::DuplicateNetworkName { name, .. }) if name == "proxy"
    ));
}

/// The same network name declared in two *different* modules stays
/// legal — each module has its own symbol table, and that's exactly what
/// makes a shared `network` definition importable. Nothing in the entry
/// file's own `networks [proxy]` reaches across the import, so the two
/// declarations never have to be told apart.
#[test]
fn same_network_name_in_two_modules_is_fine() {
    let mut loader = InMemoryLoader::default();
    loader.add("docker.hll", "network proxy {\n  external\n}\n");
    loader.add(
        "service.hll",
        "use \"docker.hll\" as traefik\n\
         network proxy {\n  external\n}\n\
         service s {\n  image \"x\"\n  networks [proxy]\n}\n",
    );

    let composed = link(Path::new("service.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));
    assert_eq!(composed.networks.len(), 1);
}

/// #71: ...but the moment the entry file *does* reach across the import
/// while declaring its own `proxy`, the two become indistinguishable.
/// Codegen re-resolves `networks [...]` by bare name against one flat
/// list where the entry file's declaration comes first, so asking for
/// `ext.proxy` used to silently produce the local `proxy` — wrong
/// Compose output, and no `traefik.docker.network` label at all, with no
/// diagnostic anywhere.
#[test]
fn imported_network_colliding_with_a_local_one_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "net.hll",
        "network proxy {\n  external\n  name: \"imported_real_name\"\n}\n",
    );
    loader.add(
        "svc.hll",
        "use \"net.hll\" as ext\n\
         network proxy {\n  name: \"local_real_name\"\n}\n\
         service web {\n  image \"nginx\"\n  networks [ext.proxy]\n}\n",
    );

    let err = link(Path::new("svc.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        &err,
        LinkError::Compose(ComposeError::CollidingImportedNetwork { alias, name, .. })
            if alias == "ext" && name == "proxy"
    ));
    // Points at the reference in the entry file, not at the imported
    // declaration — a `Span` carries no file identity, so the imported
    // one's line/column would be read against the wrong source.
    let LinkError::Compose(compose_err) = &err else {
        panic!("expected a compose error, got {err:?}");
    };
    let span = compose_err.span();
    assert_eq!((span.line, span.col), (7, 13));
}

/// The same collision between two *imported* modules: neither is the
/// entry file's own, but codegen still can't tell them apart.
#[test]
fn two_imported_networks_sharing_a_bare_name_is_error() {
    let mut loader = InMemoryLoader::default();
    loader.add("a.hll", "network proxy {\n  name: \"a_real\"\n}\n");
    loader.add("b.hll", "network proxy {\n  name: \"b_real\"\n}\n");
    loader.add(
        "svc.hll",
        "use \"a.hll\" as a\n\
         use \"b.hll\" as b\n\
         service s {\n  image \"x\"\n  networks [a.proxy, b.proxy]\n}\n",
    );

    let err = link(Path::new("svc.hll"), &loader).expect_err("expected a link error");
    assert!(matches!(
        err,
        LinkError::Compose(ComposeError::CollidingImportedNetwork { alias, name, .. })
            if alias == "b" && name == "proxy"
    ));
}

/// One imported network reached from several places is one network, not
/// a collision with itself — the check compares declarations, not just
/// how many times a reference pulled one in.
#[test]
fn one_imported_network_referenced_by_several_services_is_fine() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "net.hll",
        "network proxy {\n  external\n  name: \"real\"\n}\n",
    );
    loader.add(
        "svc.hll",
        "use \"net.hll\" as ext\n\
         template t {\n  networks [ext.proxy]\n}\n\
         service web {\n  image \"x\"\n  with t\n  networks [ext.proxy]\n}\n\
         service api {\n  image \"x\"\n  networks [ext.proxy]\n}\n",
    );

    let composed = link(Path::new("svc.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "real"
    );
}

/// A local network that shares no name with anything imported is
/// untouched by the check — both survive into the output.
#[test]
fn local_and_imported_networks_with_distinct_names_both_survive() {
    let mut loader = InMemoryLoader::default();
    loader.add(
        "net.hll",
        "network shared {\n  external\n  name: \"real\"\n}\n",
    );
    loader.add(
        "svc.hll",
        "use \"net.hll\" as ext\n\
         network internal {\n  name: \"internal_real\"\n}\n\
         service s {\n  image \"x\"\n  networks [internal, ext.shared]\n}\n",
    );

    let composed = link(Path::new("svc.hll"), &loader)
        .unwrap_or_else(|err| panic!("unexpected link error: {err}"));
    let names: Vec<&str> = composed
        .networks
        .iter()
        .map(|n| n.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["internal", "shared"]);
}
