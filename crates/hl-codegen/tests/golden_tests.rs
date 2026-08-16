//! Golden integration tests: generate Compose YAML from real hl-lang
//! fixtures and check it against the shape of the actual, currently
//! deployed homelab services those fixtures are modeled on. Comparisons
//! are between *parsed* YAML values, not raw strings — `serde_yaml`'s
//! scalar-quoting choices don't need to match the real files
//! byte-for-byte, only semantically.

use hl_codegen::{CodegenError, generate};
use hl_parser::{compose, parse};

const SYNCTHING: &str = include_str!("../../hl-parser/tests/fixtures/syncthing.hll");
const RAW_SERVICE: &str = include_str!("../../hl-parser/tests/fixtures/raw_service.hll");
const JELLYFIN: &str = include_str!("../../hl-parser/tests/fixtures/jellyfin.hll");

fn generate_from(source: &str) -> String {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    let composed = compose(program).unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    generate(composed)
        .unwrap_or_else(|err| panic!("unexpected codegen error: {err}"))
        .yaml
}

fn generate_err(source: &str) -> CodegenError {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    let composed = compose(program).unwrap_or_else(|err| panic!("unexpected compose error: {err}"));
    generate(composed).expect_err("expected a codegen error")
}

fn assert_yaml_eq(actual: &str, expected: &str) {
    let a: serde_yaml::Value = serde_yaml::from_str(actual)
        .unwrap_or_else(|err| panic!("actual output isn't valid YAML: {err}\n{actual}"));
    let e: serde_yaml::Value = serde_yaml::from_str(expected).unwrap();
    assert_eq!(
        a, e,
        "\n--- actual ---\n{actual}\n--- expected ---\n{expected}"
    );
}

/// The design doc's own worked composition example, checked against the
/// real, currently-deployed `syncthing/docker-compose.yml` this
/// milestone was grounded in. Not byte-for-byte — the fixture doesn't
/// express the real file's `TZ` env var or its extra bind mount, and per
/// the confirmed decision this codegen always emits `expose:` even
/// though the real file omits it — but everything the fixture *does*
/// express should come out matching the real file's shape exactly.
#[test]
fn syncthing_matches_real_deployed_service() {
    let yaml = generate_from(SYNCTHING);
    assert_yaml_eq(
        &yaml,
        r#"
services:
  syncthing:
    image: lscr.io/linuxserver/syncthing:latest
    container_name: syncthing
    restart: unless-stopped
    environment:
      - PUID=1000
      - PGID=100
    volumes:
      - syncthing-config:/config
    networks:
      - traefik-net
    expose:
      - 8384
    labels:
      - "traefik.docker.network=docker_default"
      - "traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)"
      - "traefik.http.routers.syncthing.entrypoints=web-secure"
      - "traefik.http.routers.syncthing.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file"
      - "traefik.http.services.syncthing.loadbalancer.server.port=8384"

networks:
  traefik-net:
    name: docker_default
    external: true

volumes:
  syncthing-config:
"#,
    );
}

/// `raw`'s job: entries land as sibling top-level service keys, matching
/// the real `cadvisor/docker-compose.yml`'s `privileged`/`devices`/
/// `security_opt` shape exactly (not asserting label parity — the real
/// file's label has a typo, `traefiki.docker.network`, that's a bug in
/// the source data, not a codegen target).
#[test]
fn cadvisor_raw_passthrough_matches_real_service() {
    let yaml = generate_from(RAW_SERVICE);
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    container_name: cadvisor
    privileged: true
    devices:
      - /dev/kmsg
    security_opt:
      seccomp: unconfined
"#,
    );
}

/// A plain service with no templates, no networks, no middleware — the
/// minimal path should still produce a valid, complete Compose doc.
#[test]
fn jellyfin_plain_service_produces_minimal_doc() {
    let yaml = generate_from(JELLYFIN);
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin:latest
    container_name: jellyfin
    restart: unless-stopped
    environment:
      - PUID=1000
    volumes:
      - /mnt/media:/data
    expose:
      - 8096
    labels:
      - "traefik.http.routers.jellyfin.rule=Host(`media.techdebtor.io`)"
      - "traefik.http.services.jellyfin.loadbalancer.server.port=8096"
"#,
    );
}

/// `container_name` (#17): defaults to the service's own name when
/// unset, so the common case — matching the syncthing/jellyfin/cadvisor
/// fixtures above — needs nothing written at all.
#[test]
fn container_name_defaults_to_service_name() {
    let yaml = generate_from("service uptime-kuma {\n  image \"louislam/uptime-kuma:latest\"\n}\n");
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        parsed["services"]["uptime-kuma"]["container_name"],
        serde_yaml::Value::String("uptime-kuma".to_string())
    );
}

/// An explicit `container_name` overrides the default — the "subdomain
/// shorter than the service name" style case the issue calls out
/// (`it-tools` -> a shorter container name), mirrored here with a
/// differently-named container.
#[test]
fn explicit_container_name_overrides_default() {
    let yaml = generate_from(
        "service it-tools {\n  image \"corentinth/it-tools:latest\"\n  container_name \"tools\"\n}\n",
    );
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        parsed["services"]["it-tools"]["container_name"],
        serde_yaml::Value::String("tools".to_string())
    );
}

/// `dns` (#14): a per-service resolver override, uptime_kuma/dashy's
/// real-world use case, now has a dedicated schema row instead of
/// routing through `raw`.
#[test]
fn dns_field_emits_dns_compose_key() {
    let yaml = generate_from(
        "service uptime-kuma {\n  \
           image \"louislam/uptime-kuma:latest\"\n  \
           dns \"192.168.50.182\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  uptime-kuma:
    image: louislam/uptime-kuma:latest
    container_name: uptime-kuma
    dns:
      - 192.168.50.182
"#,
    );
}

#[test]
fn unknown_network_reference_is_error() {
    let err = generate_err("service s {\n  image \"x\"\n  networks [nonexistent]\n}\n");
    assert!(matches!(
        err,
        CodegenError::UnknownNetwork { service, network, .. }
            if service == "s" && network == "nonexistent"
    ));
}

/// #70: the error used to carry the enclosing service's span, so an
/// undeclared network on line 4 was reported at `1:1`. It now points at
/// the offending reference itself.
#[test]
fn unknown_network_error_points_at_the_offending_reference() {
    let err = generate_err(
        "network known {\n  external\n}\n\
         service s {\n  image \"x\"\n  networks [known, nope]\n}\n",
    );
    let span = err.span();
    assert_eq!(
        (span.line, span.col),
        (6, 20),
        "expected the span of `nope`, got {}:{}",
        span.line,
        span.col
    );
}

/// The ambiguity is a property of the service's whole `networks` list —
/// no one reference is at fault — so this one deliberately keeps
/// pointing at the service.
#[test]
fn ambiguous_external_network_error_points_at_the_service() {
    let err = generate_err(
        "network a {\n  external\n}\n\
         network b {\n  external\n}\n\
         service s {\n  image \"x\"\n  networks [a, b]\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::AmbiguousExternalNetwork { ref service, .. } if service == "s"
    ));
    let span = err.span();
    assert_eq!((span.line, span.col), (7, 1));
}

#[test]
fn missing_image_is_error() {
    let err = generate_err("service s {\n}\n");
    assert!(matches!(
        err,
        CodegenError::MissingImage { service, .. } if service == "s"
    ));
}

#[test]
fn unknown_interpolation_is_error() {
    let err =
        generate_err("service s {\n  image \"x\"\n  expose 80 as \"{{typo}}.example.com\"\n}\n");
    assert!(matches!(
        err,
        CodegenError::UnknownInterpolation { binding, .. } if binding == "typo"
    ));
}

/// #65: a backtick in `expose.host` used to compile to a valid Traefik
/// rule matching every host, since `Host()`'s value has no escape for
/// its own delimiter.
#[test]
fn backtick_in_expose_host_is_error() {
    let err = generate_err(
        "service s {\n  image \"x\"\n  expose 80 as \"ok.example.com`) || HostRegexp(`{any:.+}\"\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnsafeLabelValue {
            field: "expose.host",
            character: '`',
            ..
        }
    ));
}

/// #65: `middlewares=` is a single comma-joined label, so a comma
/// inside one name silently became two references.
#[test]
fn comma_in_middleware_reference_is_error() {
    let err = generate_err(
        "service s {\n  image \"x\"\n  expose 80 as \"ok.example.com\"\n  middleware [\"a,b\"]\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnsafeLabelValue {
            field: "middleware",
            character: ',',
            ..
        }
    ));
}
