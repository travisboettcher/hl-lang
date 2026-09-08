//! Pins what `std:traefik` generates, for every shape the module can
//! produce.
//!
//! This file began as the migration gate for #271: it compiled each
//! service twice, once in the built-in `router`/`traefik` spelling and
//! once through the bundled module, and asserted the two documents were
//! equal byte for byte. That comparison is what made removing the
//! built-ins a walk-back rather than a feature deletion — evidence that
//! the template already said the same thing.
//!
//! #271 removed the other side of it. What survives is the half that
//! still has something to say: the module's output, pinned whole. Whole
//! document rather than the label list alone, because a service that
//! routes also carries `expose:` and `networks:` a reader would notice
//! moving, and a failure prints both documents rather than a boolean,
//! because the interesting outcome is *what* changed.

use hl_linker::InMemoryLoader;

/// Compiles one source through the real pipeline, with the real bundled
/// registry, and hands back the generated document.
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

/// Asserts one source generates exactly `expected`.
fn assert_generates(case: &str, source: &str, expected: &str) {
    let actual = build(source);
    assert_eq!(
        actual, expected,
        "`{case}`: `std:traefik` generated a different document\n\
         \n--- actual ---\n{actual}\n--- expected ---\n{expected}"
    );
}

/// The shape the guide teaches first: one router, a host, a port. The
/// composite writes `expose` as well as the labels, which is what keeps
/// the port written once.
#[test]
fn one_unnamed_http_router() {
    assert_generates(
        "one_unnamed_http_router",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  with traefik.http { host: \"web.example.com\", port: 8123 }\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web.rule=Host(`web.example.com`)\n    - traefik.http.services.web.loadbalancer.server.port=8123\n",
    );
}

/// The named-router composite, the same shape one level along: the
/// caller names the router and the module builds the `<service>-<name>`
/// id from it.
#[test]
fn one_named_http_router() {
    assert_generates(
        "one_named_http_router",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  with traefik.http_named { router: \"api\", host: \"api.example.com\", port: 8123 }\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web-api.rule=Host(`api.example.com`)\n    - traefik.http.services.web.loadbalancer.server.port=8123\n",
    );
}

/// `traefik.docker.network` is a template like any other since #271 —
/// the compiler no longer derives it — so the caller names the network
/// it wants and `{{net.name}}` reads that declaration's real Docker
/// name (#275). Two external networks are no longer ambiguous, because
/// nothing has to pick one any more.
#[test]
fn an_external_network_gets_its_label() {
    assert_generates(
        "an_external_network_gets_its_label",
        "use \"std:traefik\" as traefik\nnetwork proxy {\n  external\n  name: \"docker_default\"\n}\nservice web {\n  image \"nginx\"\n  networks [proxy]\n  with traefik.http { host: \"web.example.com\", port: 8123 }, traefik.docker_network { net: proxy }\n}\n",
        "services:\n  web:\n    image: nginx\n    networks:\n    - proxy\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web.rule=Host(`web.example.com`)\n    - traefik.http.services.web.loadbalancer.server.port=8123\n    - traefik.docker.network=docker_default\nnetworks:\n  proxy:\n    name: docker_default\n    external: true\n",
    );
}

/// Every optional label a router can carry, at once, on a named router
/// (#184, #221, #225): entrypoints and middlewares comma-joined from
/// lists (#283), a priority, and a TCP router beside it with a service
/// of its own.
#[test]
fn the_whole_router_feature_set_across_both_namespaces() {
    assert_generates(
        "the_whole_router_feature_set_across_both_namespaces",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  expose 8123\n  with\n    traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    traefik.http_entrypoints { router: \"{{name}}-a\", entrypoints: [\"web-secure\", \"web\"] },\n    traefik.http_middlewares { router: \"{{name}}-a\", middlewares: [\"auth@file\", \"compress@file\"] },\n    traefik.http_priority { router: \"{{name}}-a\", priority: 42 },\n    traefik.tcp_rule { router: \"{{name}}-t\", rule: \"HostSNI(`t.example.com`)\" },\n    traefik.tcp_service { router: \"{{name}}-t\", port: 7000 },\n    traefik.port { port: 8123 }\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web-a.rule=Host(`a.example.com`)\n    - traefik.http.routers.web-a.entrypoints=web-secure,web\n    - traefik.http.routers.web-a.middlewares=auth@file,compress@file\n    - traefik.http.routers.web-a.priority=42\n    - traefik.tcp.routers.web-t.rule=HostSNI(`t.example.com`)\n    - traefik.tcp.routers.web-t.service=web-t\n    - traefik.tcp.services.web-t.loadbalancer.server.port=7000\n    - traefik.http.services.web.loadbalancer.server.port=8123\n",
    );
}

/// The TCP set carrying every label it can, which is what keeps the
/// five `tcp_*` templates honest: they're the HTTP five with one segment
/// changed, and a typo in a label key there would otherwise ship
/// silently, since nothing else here writes `tcp_entrypoints`,
/// `tcp_middlewares` or `tcp_priority`.
#[test]
fn the_tcp_set_carrying_every_label() {
    assert_generates(
        "the_tcp_set_carrying_every_label",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  with\n    traefik.tcp_rule { router: \"{{name}}-t\", rule: \"HostSNI(`t.example.com`)\" },\n    traefik.tcp_entrypoints { router: \"{{name}}-t\", entrypoints: [\"tcp-secure\", \"tcp\"] },\n    traefik.tcp_middlewares { router: \"{{name}}-t\", middlewares: [\"ipallow@file\"] },\n    traefik.tcp_priority { router: \"{{name}}-t\", priority: 7 },\n    traefik.tcp_service { router: \"{{name}}-t\", port: 7000 }\n}\n",
        "services:\n  web:\n    image: nginx\n    labels:\n    - traefik.tcp.routers.web-t.rule=HostSNI(`t.example.com`)\n    - traefik.tcp.routers.web-t.entrypoints=tcp-secure,tcp\n    - traefik.tcp.routers.web-t.middlewares=ipallow@file\n    - traefik.tcp.routers.web-t.priority=7\n    - traefik.tcp.routers.web-t.service=web-t\n    - traefik.tcp.services.web-t.loadbalancer.server.port=7000\n",
    );
}

/// Two routers on one service (#184), sharing the service-wide port.
/// The `with`-list names one template twice, which is what makes the
/// fine-grained set able to express "n of these" without a template per
/// n.
#[test]
fn two_named_routers_sharing_one_port() {
    assert_generates(
        "two_named_routers_sharing_one_port",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  expose 8123\n  with\n    traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    traefik.http_rule { router: \"{{name}}-b\", rule: \"Host(`b.example.com`)\" },\n    traefik.port { port: 8123 }\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web-a.rule=Host(`a.example.com`)\n    - traefik.http.routers.web-b.rule=Host(`b.example.com`)\n    - traefik.http.services.web.loadbalancer.server.port=8123\n",
    );
}

/// A router naming its own port gets a Traefik service of its own
/// (#225) — the `.service=` pointer plus that service's port, the pair
/// `http_service` writes together, since writing one without the other
/// is never right.
#[test]
fn a_router_with_its_own_service_port() {
    assert_generates(
        "a_router_with_its_own_service_port",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  with\n    traefik.http_rule { router: \"{{name}}-a\", rule: \"Host(`a.example.com`)\" },\n    traefik.http_service { router: \"{{name}}-a\", port: 9000 }\n}\n",
        "services:\n  web:\n    image: nginx\n    labels:\n    - traefik.http.routers.web-a.rule=Host(`a.example.com`)\n    - traefik.http.routers.web-a.service=web-a\n    - traefik.http.services.web-a.loadbalancer.server.port=9000\n",
    );
}

/// `traefik.disable` (#159) is one label and nothing else.
#[test]
fn a_disabled_service() {
    assert_generates(
        "a_disabled_service",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  with traefik.disable\n}\n",
        "services:\n  web:\n    image: nginx\n    labels:\n    - traefik.enable=false\n",
    );
}

/// A rule written out rather than built from a host. Since #271 the
/// compiler no longer parses rule syntax at all, so this is the one
/// spelling there is — and the backticks Traefik wants are ordinary
/// string content.
#[test]
fn an_explicit_rule_expression() {
    assert_generates(
        "an_explicit_rule_expression",
        "use \"std:traefik\" as traefik\nservice web {\n  image \"nginx\"\n  expose 80\n  with\n    traefik.http_rule { router: \"{{name}}\", rule: \"Host(`web.example.com`) && !PathPrefix(`/admin`)\" },\n    traefik.port { port: 80 }\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 80\n    labels:\n    - traefik.http.routers.web.rule=Host(`web.example.com`) && !PathPrefix(`/admin`)\n    - traefik.http.services.web.loadbalancer.server.port=80\n",
    );
}

/// The composition the built-in `router.middleware` used to carry, and
/// the reason #288 exists: two independent templates each contribute one
/// middleware to the same key, because a list-valued `labels` entry
/// concatenates across tiers instead of colliding.
#[test]
fn two_templates_each_adding_one_middleware() {
    assert_generates(
        "two_templates_each_adding_one_middleware",
        "use \"std:traefik\" as traefik\ntemplate internal_web {\n  labels { \"traefik.http.routers.{{name}}.middlewares\": [\"local-ipwhitelist@file\"] }\n}\ntemplate authenticated {\n  labels { \"traefik.http.routers.{{name}}.middlewares\": [\"forwardAuth-authentik@file\"] }\n}\nservice web {\n  image \"nginx\"\n  with traefik.http { host: \"web.example.com\", port: 8123 }, internal_web, authenticated\n}\n",
        "services:\n  web:\n    image: nginx\n    expose:\n    - 8123\n    labels:\n    - traefik.http.routers.web.rule=Host(`web.example.com`)\n    - traefik.http.services.web.loadbalancer.server.port=8123\n    - traefik.http.routers.web.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file\n",
    );
}
