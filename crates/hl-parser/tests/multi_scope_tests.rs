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
    ComposeError, Ident, Network, Service, Span, SymbolResolver, TemplateDecl, TopDecl,
    compose_with_resolver, parse,
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

    fn resolve_defaults(&self, scope: Scope) -> Option<(Scope, &TemplateDecl)> {
        self.modules[&scope]
            .templates
            .get("defaults")
            .map(|decl| (scope, decl))
    }

    fn resolve_qualified_network(
        &self,
        scope: Scope,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Network, ComposeError> {
        let target_scope = *self.modules[&scope]
            .aliases
            .get(&qualifier.name)
            .ok_or_else(|| ComposeError::UnknownAlias {
                alias: qualifier.name.clone(),
                span: qualifier.span,
            })?;
        self.modules[&target_scope]
            .networks
            .get(name)
            .ok_or_else(|| ComposeError::UnknownTemplate {
                name: name.to_string(),
                span,
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
    assert!(network_ref.qualifier.is_none());
    assert_eq!(network_ref.name, "traefik-net");
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
