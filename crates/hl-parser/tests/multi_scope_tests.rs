//! Correctness proof for [`hl_parser::compose_with_resolver`]'s
//! generalization over [`SymbolResolver`], using a hand-rolled
//! `FakeResolver` with in-memory scopes — no real files, no `hl-linker`
//! (that's a later stage). This is the mandatory proof, before any real
//! cross-file loading exists, that:
//!
//! - two different scopes each declaring a same-named template resolve
//!   completely independently (the `cache` key must include `Scope`, not
//!   just the template name);
//! - two unrelated, non-cyclic same-named templates in different scopes,
//!   both mid-resolution in the same recursive window, don't trip a
//!   false-positive `TemplateCycle` (the `in_progress` key must include
//!   `Scope` too);
//! - a template's own qualified reference resolves using *its own*
//!   declaring scope, never the scope of whoever invoked it (the
//!   docs/DESIGN.md import-scoping rule: lexical, not caller, scoping).

use std::collections::HashMap;

use hl_parser::{
    ComposeError, ComposedProgram, Ident, Network, Service, Span, SymbolResolver, TemplateDecl,
    TopDecl, Volume, compose_with_resolver, parse,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Scope {
    Service,
    A,
    B,
    Docker,
    Decoy,
    Templates,
}

#[derive(Default)]
struct Module {
    templates: HashMap<String, TemplateDecl>,
    networks: HashMap<String, Network>,
    volumes: HashMap<String, Volume>,
    aliases: HashMap<String, Scope>,
}

struct FakeResolver {
    modules: HashMap<Scope, Module>,
}

impl SymbolResolver for FakeResolver {
    type Scope = Scope;

    fn resolve_template(
        &self,
        scope: Scope,
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
    ) -> Result<(Scope, &TemplateDecl), ComposeError> {
        let target_scope = match qualifier {
            Some(q) => *self.modules[&scope].aliases.get(&q.name).ok_or_else(|| {
                ComposeError::UnknownAlias {
                    alias: q.name.clone(),
                    span: q.span,
                }
            })?,
            None => scope,
        };
        self.modules[&target_scope]
            .templates
            .get(name)
            .map(|decl| (target_scope, decl))
            .ok_or_else(|| ComposeError::UnknownTemplate {
                name: name.to_string(),
                span,
            })
    }

    fn resolve_qualified_network(
        &self,
        scope: Scope,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Network, ComposeError> {
        let target_scope = self.alias_target(scope, qualifier)?;
        // `UnknownQualifiedNetwork`, matching `hl_linker`'s own
        // resolver, and load-bearing since #275: a field access asks
        // this first and falls through to `resolve_qualified_volume` on
        // exactly this variant, since `alias.proxy.name` says nothing
        // about which of the two kinds `proxy` is.
        self.modules[&target_scope]
            .networks
            .get(name)
            .ok_or_else(|| ComposeError::UnknownQualifiedNetwork {
                alias: qualifier.name.clone(),
                name: name.to_string(),
                span,
            })
    }

    fn resolve_qualified_volume(
        &self,
        scope: Scope,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Volume, ComposeError> {
        let target_scope = self.alias_target(scope, qualifier)?;
        self.modules[&target_scope]
            .volumes
            .get(name)
            .ok_or_else(|| ComposeError::UnknownQualifiedVolume {
                alias: qualifier.name.clone(),
                name: name.to_string(),
                span,
            })
    }
}

impl FakeResolver {
    fn alias_target(&self, scope: Scope, qualifier: &Ident) -> Result<Scope, ComposeError> {
        self.modules[&scope]
            .aliases
            .get(&qualifier.name)
            .copied()
            .ok_or_else(|| ComposeError::UnknownAlias {
                alias: qualifier.name.clone(),
                span: qualifier.span,
            })
    }
}

fn parse_template(source: &str, name: &str) -> TemplateDecl {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    for decl in program.decls {
        if let TopDecl::Template(t) = decl
            && t.name.name == name
        {
            return *t;
        }
    }
    panic!("template `{name}` not found in source: {source}");
}

fn parse_network(source: &str, name: &str) -> Network {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    for decl in program.decls {
        if let TopDecl::Network(n) = decl
            && n.name.name == name
        {
            return n;
        }
    }
    panic!("network `{name}` not found in source: {source}");
}

fn parse_volume(source: &str, name: &str) -> Volume {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    for decl in program.decls {
        if let TopDecl::Volume(v) = decl
            && v.name.name == name
        {
            return v;
        }
    }
    panic!("no volume named {name} in source");
}

fn parse_service(source: &str, name: &str) -> Service {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    for decl in program.decls {
        if let TopDecl::Service(s) = decl
            && s.name.name == name
        {
            return *s;
        }
    }
    panic!("service `{name}` not found in source: {source}");
}

#[test]
fn same_named_templates_in_different_scopes_resolve_independently() {
    let tmpl_a = parse_template("template t {\n  image \"image-a\"\n}\n", "t");
    let tmpl_b = parse_template("template t {\n  image \"image-b\"\n}\n", "t");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::A,
        Module {
            templates: HashMap::from([("t".to_string(), tmpl_a)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::B,
        Module {
            templates: HashMap::from([("t".to_string(), tmpl_b)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("a".to_string(), Scope::A), ("b".to_string(), Scope::B)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };

    let s1 = parse_service("service s1 {\n  with a.t\n}\n", "s1");
    let s2 = parse_service("service s2 {\n  with b.t\n}\n", "s2");

    let composed = compose_with_resolver(
        Vec::new(),
        Vec::new(),
        vec![s1, s2],
        Scope::Service,
        &resolver,
    )
    .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));

    assert_eq!(
        composed.services[0]
            .fields
            .image
            .as_ref()
            .unwrap()
            .reference
            .as_ref()
            .unwrap()
            .text(),
        "image-a"
    );
    assert_eq!(
        composed.services[1]
            .fields
            .image
            .as_ref()
            .unwrap()
            .reference
            .as_ref()
            .unwrap()
            .text(),
        "image-b"
    );
}

#[test]
fn same_named_templates_mid_resolution_in_different_scopes_do_not_false_cycle() {
    // A's `t` invokes B's `t` (a different template, despite the same
    // name) while A's own `t` is still `in_progress` — a name-only
    // (not scope-keyed) `in_progress` check would wrongly see B's `t`
    // as a re-entry into A's `t` and report a spurious cycle.
    let tmpl_a = parse_template("template t {\n  with b.t\n  image \"image-a\"\n}\n", "t");
    let tmpl_b = parse_template("template t {\n  image \"image-b\"\n}\n", "t");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::A,
        Module {
            templates: HashMap::from([("t".to_string(), tmpl_a)]),
            aliases: HashMap::from([("b".to_string(), Scope::B)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::B,
        Module {
            templates: HashMap::from([("t".to_string(), tmpl_b)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("a".to_string(), Scope::A)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service("service s {\n  with a.t\n}\n", "s");

    let composed =
        compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| {
                panic!("unexpected compose error (false-positive cycle?): {err}")
            });

    // A's `t` own body (`image "image-a"`) always wins over its own
    // explicit `with b.t` tier — this also confirms B's `t` actually
    // got resolved and merged in, not just silently skipped.
    assert_eq!(
        composed.services[0]
            .fields
            .image
            .as_ref()
            .unwrap()
            .reference
            .as_ref()
            .unwrap()
            .text(),
        "image-a"
    );
}

#[test]
fn template_qualified_reference_resolves_in_its_own_declaring_scope_not_the_invokers() {
    // Two distinct real networks, both (confusingly) named `traefik-net`
    // at the source level, distinguished by their `real_name`. Whichever
    // one wins proves whose alias table `traefik` was looked up in.
    let docker_net = parse_network(
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n",
        "traefik-net",
    );
    let decoy_net = parse_network(
        "network traefik-net {\n  name: \"decoy_network\"\n}\n",
        "traefik-net",
    );
    let tmpl_web = parse_template(
        "template web {\n  networks [traefik.traefik-net]\n}\n",
        "web",
    );

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            networks: HashMap::from([("traefik-net".to_string(), docker_net)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Decoy,
        Module {
            networks: HashMap::from([("traefik-net".to_string(), decoy_net)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Templates,
        Module {
            templates: HashMap::from([("web".to_string(), tmpl_web)]),
            // `web`'s own declaring scope resolves `traefik` -> the
            // *real* Docker scope.
            aliases: HashMap::from([("traefik".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([
                ("templates".to_string(), Scope::Templates),
                // The invoking scope's own `traefik` alias points at
                // the *decoy* — if resolution incorrectly used the
                // caller's scope instead of `web`'s own, this is the
                // one that would win.
                ("traefik".to_string(), Scope::Decoy),
            ]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service("service s {\n  with templates.web\n}\n", "s");

    let composed =
        compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));

    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default",
        "expected the template's own declaring scope's `traefik` alias (Docker) to win, not the invoking service's (Decoy)"
    );

    let network_ref = &composed.services[0].fields.networks[0];
    assert!(network_ref.qualifier().is_none());
    assert_eq!(network_ref.text(), "traefik-net");
}

// --- imported-network name collisions (#71) ---
//
// These live here rather than only in `hl-linker`'s `link_tests.rs`
// because the check itself lives in `compose_with_resolver`: a qualified
// network reference is unreachable through the plain `compose()` entry
// point (`SingleFileResolver` has no aliases at all), so `FakeResolver`
// is the only way to exercise this code from inside `hl-parser`, where
// it's written.

/// The entry file declares its own `proxy` *and* reaches across an
/// import for another one. Codegen resolves `networks [...]` by bare
/// name against one flat list, so the two are indistinguishable there
/// and the entry file's own silently wins — which is the silent
/// wrongness this rejects.
#[test]
fn imported_network_colliding_with_an_entry_network_is_error() {
    let imported = parse_network(
        "network proxy {\n  external\n  name: \"imported_real\"\n}\n",
        "proxy",
    );
    let local = parse_network("network proxy {\n  name: \"local_real\"\n}\n", "proxy");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            networks: HashMap::from([("proxy".to_string(), imported)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("ext".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service(
        "service s {\n  image \"x\"\n  networks [ext.proxy]\n}\n",
        "s",
    );

    let err = compose_with_resolver(vec![local], Vec::new(), vec![s], Scope::Service, &resolver)
        .expect_err("expected a compose error");
    assert!(
        matches!(
            &err,
            ComposeError::CollidingImportedNetwork { alias, name, .. }
                if alias == "ext" && name == "proxy"
        ),
        "expected CollidingImportedNetwork, got {err:?}"
    );
    // The reference in the entry file, not the imported declaration:
    // a `Span` carries no file identity.
    assert_eq!((err.span().line, err.span().col), (3, 13));
}

/// Two *imported* networks sharing a bare name collide with each other
/// for the same reason, with no entry-file declaration involved.
#[test]
fn two_imported_networks_sharing_a_bare_name_is_error() {
    let net_a = parse_network("network proxy {\n  name: \"a_real\"\n}\n", "proxy");
    let net_b = parse_network("network proxy {\n  name: \"b_real\"\n}\n", "proxy");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::A,
        Module {
            networks: HashMap::from([("proxy".to_string(), net_a)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::B,
        Module {
            networks: HashMap::from([("proxy".to_string(), net_b)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("a".to_string(), Scope::A), ("b".to_string(), Scope::B)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service(
        "service s {\n  image \"x\"\n  networks [a.proxy, b.proxy]\n}\n",
        "s",
    );

    let err = compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
        .expect_err("expected a compose error");
    assert!(
        matches!(
            &err,
            ComposeError::CollidingImportedNetwork { alias, name, .. }
                if alias == "b" && name == "proxy"
        ),
        "expected CollidingImportedNetwork naming `b`, got {err:?}"
    );
}

/// One imported network pulled in by several references is the same
/// declaration reaching the merge more than once, not a collision with
/// itself — this is what the equality check in that arm is for, and it's
/// why the check compares declarations rather than just counting names.
#[test]
fn one_imported_network_pulled_in_twice_is_not_a_collision() {
    let imported = parse_network(
        "network proxy {\n  external\n  name: \"real\"\n}\n",
        "proxy",
    );

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            networks: HashMap::from([("proxy".to_string(), imported)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("ext".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s1 = parse_service(
        "service s1 {\n  image \"x\"\n  networks [ext.proxy]\n}\n",
        "s1",
    );
    let s2 = parse_service(
        "service s2 {\n  image \"x\"\n  networks [ext.proxy]\n}\n",
        "s2",
    );

    let composed = compose_with_resolver(
        Vec::new(),
        Vec::new(),
        vec![s1, s2],
        Scope::Service,
        &resolver,
    )
    .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));

    // Pulled in twice, present once — the duplicate is recognized as the
    // same declaration and dropped rather than appended.
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "real"
    );
}

/// An entry-file network whose name nothing imported shares is left
/// alone, and an imported one under a different name joins it.
#[test]
fn entry_and_imported_networks_with_distinct_names_both_survive() {
    let imported = parse_network(
        "network shared {\n  external\n  name: \"real\"\n}\n",
        "shared",
    );
    let local = parse_network(
        "network internal {\n  name: \"internal_real\"\n}\n",
        "internal",
    );

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            networks: HashMap::from([("shared".to_string(), imported)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("ext".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service(
        "service s {\n  image \"x\"\n  networks [internal, ext.shared]\n}\n",
        "s",
    );

    let composed =
        compose_with_resolver(vec![local], Vec::new(), vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));

    let names: Vec<&str> = composed
        .networks
        .iter()
        .map(|n| n.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["internal", "shared"]);
}
/// The volume side of the same contract: a qualified named-volume host
/// resolves through the resolver's own alias table, gets rewritten to
/// the resolved declaration's bare name, and pulls that declaration into
/// the composed program alongside the entry scope's own.
#[test]
fn qualified_volume_host_resolves_and_pulls_its_declaration_in() {
    let imported = parse_volume(
        "volume media {\n  external\n  name: \"media_store\"\n}\n",
        "media",
    );
    let local = parse_volume("volume config {}\n", "config");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            volumes: HashMap::from([("media".to_string(), imported)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("storage".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service(
        "service s {\n  \
           image \"x\"\n  \
           volume config -> \"/config\"\n  \
           volume storage.media -> \"/data\"\n\
         }\n",
        "s",
    );

    let composed =
        compose_with_resolver(Vec::new(), vec![local], vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));

    let names: Vec<&str> = composed
        .volumes
        .iter()
        .map(|v| v.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["config", "media"]);
    let hosts: Vec<&str> = composed.services[0]
        .fields
        .volumes
        .entries
        .iter()
        .map(|e| e.host.text())
        .collect();
    assert_eq!(hosts, vec!["config", "media"]);
}

/// And the collision rule reaches volumes too: the entry scope's own
/// `media` and an imported `media` are one Compose key claimed by two
/// declarations.
#[test]
fn imported_volume_colliding_with_an_entry_volume_is_error() {
    let imported = parse_volume("volume media {\n  name: \"imported_real\"\n}\n", "media");
    let local = parse_volume("volume media {\n  name: \"local_real\"\n}\n", "media");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            volumes: HashMap::from([("media".to_string(), imported)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([("storage".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );

    let resolver = FakeResolver { modules };
    let s = parse_service(
        "service s {\n  image \"x\"\n  volume storage.media -> \"/data\"\n}\n",
        "s",
    );

    let err = compose_with_resolver(Vec::new(), vec![local], vec![s], Scope::Service, &resolver)
        .expect_err("expected a compose error");
    assert!(
        matches!(
            &err,
            ComposeError::CollidingImportedVolume { alias, name, .. }
                if alias == "storage" && name == "media"
        ),
        "expected CollidingImportedVolume, got {err:?}"
    );
}

// --- reading an imported declaration's real name (#275) ---
//
// `alias.decl.name` is the third spelling of a field access, and the
// only one that needs a second scope to mean anything — so these live
// here for the reason the qualified-reference cases above do: the plain
// `compose()` entry point has no aliases at all.

/// Two networks, both called `proxy`, told apart by the Docker name
/// each resolves to — the same decoy shape
/// `template_qualified_reference_resolves_in_its_own_declaring_scope_not_the_invokers`
/// uses, because a field access has to obey that same rule: which
/// file's `traefik` alias answers is decided where the access is
/// *written*, not where the template is invoked.
fn decoy_modules(template: TemplateDecl) -> HashMap<Scope, Module> {
    let real = parse_network(
        "network proxy {\n  external\n  name: \"docker_default\"\n}\n",
        "proxy",
    );
    let decoy = parse_network("network proxy {\n  name: \"decoy_network\"\n}\n", "proxy");
    let media = parse_volume("volume media {\n  name: \"media_store\"\n}\n", "media");

    let mut modules = HashMap::new();
    modules.insert(
        Scope::Docker,
        Module {
            networks: HashMap::from([("proxy".to_string(), real)]),
            volumes: HashMap::from([("media".to_string(), media)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Decoy,
        Module {
            networks: HashMap::from([("proxy".to_string(), decoy)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Templates,
        Module {
            templates: HashMap::from([(template.name.name.clone(), template)]),
            aliases: HashMap::from([("traefik".to_string(), Scope::Docker)]),
            ..Default::default()
        },
    );
    modules.insert(
        Scope::Service,
        Module {
            aliases: HashMap::from([
                ("templates".to_string(), Scope::Templates),
                // The invoking scope's own `traefik` points at the
                // decoy, so a field access resolved in the caller's
                // scope reads `decoy_network` and fails the assertion.
                ("traefik".to_string(), Scope::Decoy),
            ]),
            ..Default::default()
        },
    );
    modules
}

/// Composes `service s { with templates.web }` against
/// [`decoy_modules`] and hands back the label `web` contributed.
fn decoy_label(template: TemplateDecl) -> String {
    let resolver = FakeResolver {
        modules: decoy_modules(template),
    };
    let s = parse_service("service s {\n  image \"x\"\n  with templates.web\n}\n", "s");
    let composed =
        compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    assert!(
        composed.networks.is_empty(),
        "reading a name must not pull the declaration into the program: {:?}",
        composed.networks
    );
    composed.services[0].fields.labels.entries[0]
        .value
        .text()
        .to_string()
}

#[test]
fn a_qualified_field_access_resolves_in_the_scope_it_was_written_in() {
    let template = parse_template(
        "template web {\n  labels { \"caddy.network\": traefik.proxy.name }\n}\n",
        "web",
    );
    assert_eq!(decoy_label(template), "docker_default");
}

#[test]
fn a_qualified_field_access_naming_neither_kind_is_an_error() {
    let template = parse_template(
        "template web {\n  labels { \"k\": traefik.nothing.name }\n}\n",
        "web",
    );
    let resolver = FakeResolver {
        modules: decoy_modules(template),
    };
    let s = parse_service("service s {\n  image \"x\"\n  with templates.web\n}\n", "s");
    let err = compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
        .expect_err("expected a compose error");
    match err {
        ComposeError::UnknownQualifiedDeclaration { alias, name, .. } => {
            assert_eq!(alias, "traefik");
            assert_eq!(name, "nothing");
        }
        other => panic!("expected UnknownQualifiedDeclaration, got {other:?}"),
    }
}

/// An alias that resolves to nothing at all is a different mistake with
/// a different fix, so it keeps its own diagnostic rather than being
/// folded into "no such declaration."
#[test]
fn a_field_access_through_an_unknown_alias_names_the_alias() {
    let template = parse_template(
        "template web {\n  labels { \"k\": nope.proxy.name }\n}\n",
        "web",
    );
    let resolver = FakeResolver {
        modules: decoy_modules(template),
    };
    let s = parse_service("service s {\n  image \"x\"\n  with templates.web\n}\n", "s");
    let err = compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
        .expect_err("expected a compose error");
    match err {
        ComposeError::UnknownAlias { alias, .. } => assert_eq!(alias, "nope"),
        other => panic!("expected UnknownAlias, got {other:?}"),
    }
}

/// The interpolated spelling carries the same lexical-scoping
/// requirement, and is resolved in the same place for that reason.
#[test]
fn a_qualified_interpolated_field_access_resolves_in_the_same_scope() {
    let template = parse_template(
        "template web {\n  labels { \"caddy.network\": \"x-{{traefik.proxy.name}}\" }\n}\n",
        "web",
    );
    assert_eq!(decoy_label(template), "x-docker_default");
}

/// Composes `service s { with templates.web }` against
/// [`decoy_modules`], with a second template `inner` dropped into the
/// same `Templates` scope for `web` to invoke — the shape every case
/// about an *argument* needs, since an argument only means anything at
/// a call site.
///
/// `entry_networks`/`entry_volumes` are the composing program's own
/// declarations, which is how a case can put a *local* declaration in
/// the way of the alias spelling.
fn decoy_invocation(
    inner: TemplateDecl,
    web: TemplateDecl,
    entry_networks: Vec<Network>,
    entry_volumes: Vec<Volume>,
) -> Result<ComposedProgram, ComposeError> {
    let mut modules = decoy_modules(web);
    modules
        .get_mut(&Scope::Templates)
        .expect("templates module")
        .templates
        .insert("inner".to_string(), inner);

    let resolver = FakeResolver { modules };
    let s = parse_service("service s {\n  image \"x\"\n  with templates.web\n}\n", "s");
    compose_with_resolver(
        entry_networks,
        entry_volumes,
        vec![s],
        Scope::Service,
        &resolver,
    )
}

/// [`decoy_invocation`]'s first label, for the cases that expect one.
fn decoy_invocation_label(inner: TemplateDecl, web: TemplateDecl) -> String {
    let composed = decoy_invocation(inner, web, Vec::new(), Vec::new())
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    composed.services[0].fields.labels.entries[0]
        .value
        .text()
        .to_string()
}

/// A `with`-invocation's arguments are values written at the call site,
/// so one written inside a template resolves against that template's
/// own file — which is why field access is resolved before the
/// `with`-list rather than after it.
#[test]
fn a_qualified_field_access_in_an_invocation_argument_uses_the_writing_scope() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": $n }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy.name }\n}\n",
        "web",
    );
    assert_eq!(decoy_invocation_label(inner, web), "docker_default");
}

/// #296: a two-segment `alias.decl` argument names the *declaration*,
/// which the callee then reads a field off — and the field resolves in
/// the scope the argument was written in, not the invoking service's,
/// whose own `traefik` alias points at the decoy.
#[test]
fn an_imported_declaration_argument_is_read_in_the_writing_scope() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": \"{{n.name}}\" }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy }\n}\n",
        "web",
    );
    assert_eq!(decoy_invocation_label(inner, web), "docker_default");
}

/// The `$param.field` spelling of the same read, which substitution
/// binds through a different path than the interpolated one.
#[test]
fn an_imported_declaration_argument_answers_a_slot_field_access() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": $n.name }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy }\n}\n",
        "web",
    );
    assert_eq!(decoy_invocation_label(inner, web), "docker_default");
}

/// An undotted `{{n}}` interpolates the declaration's own identifier —
/// the bare name it is reached by once imported, which is exactly what a
/// same-file declaration passed the same way contributes.
#[test]
fn an_imported_declaration_argument_interpolates_as_its_bare_name() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": \"{{n}}\" }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy }\n}\n",
        "web",
    );
    assert_eq!(decoy_invocation_label(inner, web), "proxy");
}

/// A parameter bound to an imported declaration reaches `networks
/// [$net]` too, and attaching it there imports the declaration exactly
/// as a written `networks [alias.name]` would — bare reference in the
/// service, declaration in the program.
#[test]
fn an_imported_declaration_argument_attaches_the_network_it_names() {
    let inner = parse_template("template inner(n) {\n  networks [$n]\n}\n", "inner");
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy }\n}\n",
        "web",
    );
    let composed = decoy_invocation(inner, web, Vec::new(), Vec::new())
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    let network_ref = &composed.services[0].fields.networks[0];
    assert!(network_ref.qualifier().is_none());
    assert_eq!(network_ref.text(), "proxy");
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(
        composed.networks[0].real_name.as_ref().unwrap().text(),
        "docker_default"
    );
}

/// A list argument carries one item by item, so an imported
/// declaration splices into `networks [$nets]` beside a same-file name
/// and is imported exactly as a written entry would be.
#[test]
fn an_imported_declaration_splices_out_of_a_list_argument() {
    let inner = parse_template("template inner(nets) {\n  networks [$nets]\n}\n", "inner");
    let web = parse_template(
        "template web {\n  with inner { nets: [traefik.proxy] }\n}\n",
        "web",
    );
    let composed = decoy_invocation(inner, web, Vec::new(), Vec::new())
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    let network_ref = &composed.services[0].fields.networks[0];
    assert!(network_ref.qualifier().is_none());
    assert_eq!(network_ref.text(), "proxy");
    assert_eq!(composed.networks.len(), 1);
}

/// Forwarding one on through a second `with` hop reaches the same
/// answer: the innermost read still resolves against the file that
/// wrote `traefik.proxy`, however many parameters it was renamed
/// through on the way.
#[test]
fn a_forwarded_imported_declaration_argument_still_reads_in_the_writing_scope() {
    let inner = parse_template(
        "template inner(i) {\n  labels { \"caddy.network\": \"{{i.name}}\" }\n}\n",
        "inner",
    );
    let mid = parse_template("template mid(n) {\n  with inner { i: $n }\n}\n", "mid");
    let web = parse_template(
        "template web {\n  with mid { n: traefik.proxy }\n}\n",
        "web",
    );
    let mut modules = decoy_modules(web);
    let templates = &mut modules
        .get_mut(&Scope::Templates)
        .expect("templates module")
        .templates;
    templates.insert("inner".to_string(), inner);
    templates.insert("mid".to_string(), mid);

    let resolver = FakeResolver { modules };
    let s = parse_service("service s {\n  image \"x\"\n  with templates.web\n}\n", "s");
    let composed =
        compose_with_resolver(Vec::new(), Vec::new(), vec![s], Scope::Service, &resolver)
            .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    assert_eq!(
        composed.services[0].fields.labels.entries[0].value.text(),
        "docker_default"
    );
}

/// A declaration is not a value: passing one to a parameter the callee
/// splices into an ordinary field says so at the argument, rather than
/// letting a reference reach codegen.
#[test]
fn an_imported_declaration_argument_in_a_value_position_is_refused() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": $n }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.proxy }\n}\n",
        "web",
    );
    let err =
        decoy_invocation(inner, web, Vec::new(), Vec::new()).expect_err("expected a compose error");
    match err {
        ComposeError::QualifiedArgumentNotAValue {
            template,
            alias,
            name,
            ..
        } => {
            assert_eq!(template, "inner");
            assert_eq!(alias, "traefik");
            assert_eq!(name, "proxy");
        }
        other => panic!("expected QualifiedArgumentNotAValue, got {other:?}"),
    }
}

/// A base that names one of the program's own declarations keeps the
/// field-access reading it has always had, whatever the aliases in
/// scope: the alias spelling is only ever tried for a base that names
/// nothing local, so no access written before #296 can change meaning.
#[test]
fn a_local_declaration_wins_the_two_segment_argument_spelling() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": $n }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.name }\n}\n",
        "web",
    );
    let local = parse_network("network traefik {\n  name: \"local_net\"\n}\n", "traefik");
    let composed = decoy_invocation(inner, web, vec![local], Vec::new())
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    assert_eq!(
        composed.services[0].fields.labels.entries[0].value.text(),
        "local_net"
    );
}

/// Both kinds count as local, since both answer a field access: a
/// `volume` in the way of an alias name takes the spelling exactly as a
/// `network` does.
#[test]
fn a_local_volume_also_wins_the_two_segment_argument_spelling() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": $n }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.name }\n}\n",
        "web",
    );
    let local = parse_volume("volume traefik {\n  name: \"local_vol\"\n}\n", "traefik");
    let composed = decoy_invocation(inner, web, Vec::new(), vec![local])
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    assert_eq!(
        composed.services[0].fields.labels.entries[0].value.text(),
        "local_vol"
    );
}

/// The argument pass claims only what an argument put there, so a field
/// access bound to an ordinary same-file name still resolves where it
/// always did — over the service's *merged* fields. A contribution the
/// service's own body overrides is gone by then, and goes on drawing no
/// diagnostic, however unresolvable the access it carried.
#[test]
fn an_overridden_contributions_field_access_still_draws_no_diagnostic() {
    let inner = parse_template("template inner(n) {\n  image $n.name\n}\n", "inner");
    let web = parse_template("template web {\n  with inner { n: ghost }\n}\n", "web");
    let composed = decoy_invocation(inner, web, Vec::new(), Vec::new())
        .unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    assert_eq!(
        composed.services[0]
            .fields
            .image
            .as_ref()
            .unwrap()
            .reference
            .as_ref()
            .unwrap()
            .text(),
        "x"
    );
}

/// An alias that resolves, holding no declaration by that name, names
/// the mistake that was actually made — #296's own complaint about the
/// diagnostic, which used to send the reader hunting for a local
/// declaration they never meant to write.
#[test]
fn an_argument_naming_no_declaration_in_a_real_alias_says_so() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": \"{{n.name}}\" }\n}\n",
        "inner",
    );
    let web = parse_template(
        "template web {\n  with inner { n: traefik.nothing }\n}\n",
        "web",
    );
    let err =
        decoy_invocation(inner, web, Vec::new(), Vec::new()).expect_err("expected a compose error");
    match err {
        ComposeError::UnknownQualifiedDeclaration { alias, name, .. } => {
            assert_eq!(alias, "traefik");
            assert_eq!(name, "nothing");
        }
        other => panic!("expected UnknownQualifiedDeclaration, got {other:?}"),
    }
}

/// A base that is neither a local declaration nor an alias was a field
/// access all along, and is still reported as one.
#[test]
fn an_argument_naming_neither_a_declaration_nor_an_alias_stays_a_field_access() {
    let inner = parse_template(
        "template inner(n) {\n  labels { \"caddy.network\": \"{{n.name}}\" }\n}\n",
        "inner",
    );
    let web = parse_template("template web {\n  with inner { n: nope.name }\n}\n", "web");
    let err =
        decoy_invocation(inner, web, Vec::new(), Vec::new()).expect_err("expected a compose error");
    match err {
        ComposeError::FieldBaseNotDeclared { base, .. } => assert_eq!(base, "nope"),
        other => panic!("expected FieldBaseNotDeclared, got {other:?}"),
    }
}

/// `alias.decl.name` says nothing about which kind `decl` is, and both
/// kinds carry the field, so the volume side answers the same way.
#[test]
fn a_qualified_field_access_reads_an_imported_volumes_name() {
    let template = parse_template(
        "template web {\n  labels { \"k\": traefik.media.name }\n}\n",
        "web",
    );
    assert_eq!(decoy_label(template), "media_store");
}
