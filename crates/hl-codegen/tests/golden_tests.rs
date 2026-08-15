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

/// [`assert_yaml_eq`] compares *parsed* YAML, so it can't tell a bare
/// `name:` apart from an explicit `name: null` — both parse to the same
/// value. This checks the actual generated text instead, since the whole
/// point of emitting the bare form is avoiding a byte-level diff against
/// hand-written Compose files.
#[test]
fn driverless_named_volume_emits_bare_key_not_explicit_null() {
    let yaml = generate_from(SYNCTHING);
    assert!(
        yaml.contains("volumes:\n  syncthing-config:\n")
            || yaml.trim_end().ends_with("syncthing-config:"),
        "expected a bare `syncthing-config:` key with no explicit `null`, got:\n{yaml}"
    );
    assert!(
        !yaml.contains("syncthing-config: null"),
        "generated YAML should never spell out `: null` for a driver-less named volume, got:\n{yaml}"
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
