//! Proves `std:traefik` generates exactly what the built-in
//! `router`/`traefik` fields generate, for every shape those built-ins
//! can produce (#269).
//!
//! This is the gate the migration turns on. #271 removes `router`,
//! `traefik`, the matcher table and the rule grammar, and the only
//! thing that makes that a walk-back rather than a feature deletion is
//! evidence that the template already says the same thing. "The same
//! thing" here means the whole generated document, byte for byte —
//! not just the label list, since a service that routes also carries
//! `expose:` and `networks:` a reader would notice moving.
//!
//! Each case compiles twice: once written the built-in way, once
//! through the bundled module. A difference of any kind fails, and the
//! failure prints both documents rather than a boolean, because the
//! interesting outcome is *what* diverged.
//!
//! Paired `.hll` files under `tests/cases/` cover the same ground for a
//! reader — `issue_269_builtin_router_http.hll` next to
//! `issue_269_std_traefik_http.hll` — but a corpus case pins one
//! spelling's output rather than the two against each other, so
//! nothing there would notice the pair drifting apart. This does.

use hl_linker::InMemoryLoader;

/// Compiles one source through the real pipeline, with the real
/// bundled registry, and hands back the generated document.
///
/// `link` rather than a hand-built resolver on purpose: `std:traefik`
/// resolves through [`hl_linker`]'s own registry, so anything short of
/// the real linker would prove something about a fixture instead of
/// about the module this crate ships.
fn build(source: &str) -> String {
    let mut loader = InMemoryLoader::default();
    loader.add("main.hll", source);
    let linked = hl_linker::link(std::path::Path::new("main.hll"), &loader)
        .unwrap_or_else(|err| panic!("link failed:\n{err}\n\nsource:\n{source}"));
    hl_codegen::generate(linked.program)
        .unwrap_or_else(|err| panic!("codegen failed:\n{err:?}\n\nsource:\n{source}"))
        .yaml
}

/// Asserts the two spellings of one service generate the same document.
fn assert_same(case: &str, builtin: &str, template: &str) {
    let from_builtin = build(builtin);
    let from_template = build(template);
    assert_eq!(
        from_builtin, from_template,
        "`{case}`: the built-in and `std:traefik` spellings generated different documents\n\
         \n--- built-in ---\n{from_builtin}\n--- std:traefik ---\n{from_template}"
    );
}

/// The shape the guide teaches first: one unnamed router, a host, a
/// port. The composite writes `expose` as well as the label, which is
/// what keeps the port written once — the built-in derived its label
/// from `expose.port` for the same reason.
#[test]
fn one_unnamed_http_router() {
    assert_same(
        "one_unnamed_http_router",
        "service web {\n  image \"nginx\"\n  expose 8123\n  router {\n    host: \"web.example.com\"\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  \
         with traefik.http { host: \"web.example.com\", port: 8123 }\n}\n",
    );
}

/// `expose <port> as \"<host>\"` desugars to an unnamed router during
/// parsing, so it has to reach the same document as the long form — and
/// as the template. This is the spelling #271 removes that a reader
/// will miss most.
#[test]
fn the_expose_as_sugar() {
    assert_same(
        "the_expose_as_sugar",
        "service web {\n  image \"nginx\"\n  expose 8123 as \"web.example.com\"\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  \
         with traefik.http { host: \"web.example.com\", port: 8123 }\n}\n",
    );
}

/// An external network contributes `traefik.docker.network`, which the
/// compiler still derives (see the module's own note): the template
/// writes every other label and leaves that one alone, so the documents
/// still match. When #271 moves that label, this case is what catches
/// the move going wrong.
#[test]
fn an_external_network_still_gets_its_label() {
    assert_same(
        "an_external_network_still_gets_its_label",
        "network proxy {\n  external\n  name: \"docker_default\"\n}\n\
         service web {\n  image \"nginx\"\n  networks [proxy]\n  expose 8123\n  \
         router {\n    host: \"web.example.com\"\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         network proxy {\n  external\n  name: \"docker_default\"\n}\n\
         service web {\n  image \"nginx\"\n  networks [proxy]\n  \
         with traefik.http { host: \"web.example.com\", port: 8123 }\n}\n",
    );
}

/// Every optional label a router can carry, at once, on a named router
/// (#184, #221, #225): entrypoints and middlewares comma-joined from
/// lists (#283), a priority, and a TCP router beside it with a service
/// of its own.
#[test]
fn the_whole_router_feature_set_across_both_namespaces() {
    assert_same(
        "the_whole_router_feature_set_across_both_namespaces",
        "service web {\n  image \"nginx\"\n  expose 8123\n  \
         router a {\n    host: \"a.example.com\"\n    entrypoints: web-secure, web\n    \
         middleware [auth, compress]\n    priority: 42\n  }\n  \
         router t {\n    protocol: tcp\n    host: \"t.example.com\"\n    port: 7000\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  expose 8123\n  with\n    \
         traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    \
         traefik.http_entrypoints { router: \"{{name}}-a\", entrypoints: [\"web-secure\", \"web\"] },\n    \
         traefik.http_middlewares { router: \"{{name}}-a\", middlewares: [\"auth@file\", \"compress@file\"] },\n    \
         traefik.http_priority { router: \"{{name}}-a\", priority: 42 },\n    \
         traefik.tcp_rule { router: \"{{name}}-t\", rule: \"HostSNI(`t.example.com`)\" },\n    \
         traefik.tcp_service { router: \"{{name}}-t\", port: 7000 },\n    \
         traefik.port { port: 8123 }\n}\n",
    );
}

/// Two routers on one service (#184), sharing the service-wide port.
/// The `with`-list names one template twice, which is what makes the
/// fine-grained set able to express "n of these" without a template per
/// n.
#[test]
fn two_named_routers_sharing_one_port() {
    assert_same(
        "two_named_routers_sharing_one_port",
        "service web {\n  image \"nginx\"\n  expose 8123\n  \
         router a {\n    host: \"a.example.com\"\n  }\n  \
         router b {\n    host: \"b.example.com\"\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  expose 8123\n  with\n    \
         traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    \
         traefik.http_rule { router: \"{{name}}-b\", rule: \"Host(`b.example.com`)\" },\n    \
         traefik.port { port: 8123 }\n}\n",
    );
}

/// A router naming its own port gets a Traefik service of its own
/// (#225), which is the `.service=` pointer plus that service's port —
/// the pair `http_service` writes together, since writing one without
/// the other is never right.
#[test]
fn a_router_with_its_own_service_port() {
    assert_same(
        "a_router_with_its_own_service_port",
        "service web {\n  image \"nginx\"\n  \
         router a {\n    host: \"a.example.com\"\n    port: 9000\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  with\n    \
         traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    \
         traefik.http_service { router: \"{{name}}-a\", port: 9000 }\n}\n",
    );
}

/// `traefik { disable }` (#159) is one label and nothing else — no
/// `traefik.docker.network` either, since a container Traefik ignores
/// has no network for it to route over.
#[test]
fn a_disabled_service() {
    assert_same(
        "a_disabled_service",
        "service web {\n  image \"nginx\"\n  traefik {\n    disable\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  with traefik.disable\n}\n",
    );
}

/// A rule written out rather than built from a host, which is what the
/// `rule` field takes today and what a template caller writes directly.
/// The built-in renders its parsed expression back to text, so this is
/// also a check that the rendering and the hand-written spelling agree.
#[test]
fn an_explicit_rule_expression() {
    assert_same(
        "an_explicit_rule_expression",
        "service web {\n  image \"nginx\"\n  expose 80\n  \
         router {\n    rule: Host(\"web.example.com\") && !PathPrefix(\"/admin\")\n  }\n}\n",
        "use \"std:traefik\" as traefik\n\
         service web {\n  image \"nginx\"\n  expose 80\n  with\n    \
         traefik.http_rule { router: \"{{name}}\", \
         rule: \"Host(`web.example.com`) && !PathPrefix(`/admin`)\" },\n    \
         traefik.port { port: 80 }\n}\n",
    );
}
