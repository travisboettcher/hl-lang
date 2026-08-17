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

/// `container_name` (#90): never emitted unless `.hll` sets it
/// explicitly. Compose's own per-project default naming (`<project>_
/// <service>_1`) is what most people want, and defaulting the built-in
/// to the service's own name reliably collided across independent
/// stacks sharing a common service name (`db`, `broker`, ...) — Compose
/// refuses to start the second container with the same name.
#[test]
fn container_name_is_absent_when_unset() {
    let yaml = generate_from("service uptime-kuma {\n  image \"louislam/uptime-kuma:latest\"\n}\n");
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert!(
        parsed["services"]["uptime-kuma"]
            .as_mapping()
            .unwrap()
            .get("container_name")
            .is_none(),
        "expected no container_name key, got:\n{yaml}"
    );
}

/// An explicit `container_name` is emitted verbatim — the case the issue
/// calls out as the deliberate, opt-in use (a stable DNS name or an
/// external reference), mirrored here with a shorter container name than
/// the service's own.
#[test]
fn explicit_container_name_is_emitted() {
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

/// #69: one external network named by several tiers is one answer given
/// repeatedly, not an ambiguity between it and itself. Composition drops
/// the repeated `networks` entries, and the label resolves to the single
/// real name rather than erroring out.
#[test]
fn external_network_named_by_several_tiers_is_not_ambiguous() {
    let yaml = generate_from(
        "network proxy {\n  external\n  name: \"docker_default\"\n}\n\
         template a {\n  networks [proxy]\n}\n\
         template b {\n  networks [proxy]\n}\n\
         service web {\n  image \"nginx\"\n  with a, b\n  networks [proxy]\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        "services:\n  web:\n    image: nginx\n\
         \n    networks: [proxy]\n    labels:\n\
         \n      - traefik.docker.network=docker_default\n\
         networks:\n  proxy:\n    name: docker_default\n    external: true\n",
    );
}

/// Two distinct declarations that resolve to the *same* real name are
/// not an ambiguity either: the check is on distinct real names, so
/// there is still only one answer `traefik.docker.network` could take.
/// (Two genuinely different external networks remain an error — see
/// `ambiguous_external_network_error_points_at_the_service`.)
#[test]
fn two_declarations_sharing_one_real_name_are_not_ambiguous() {
    let yaml = generate_from(
        "network a {\n  external\n  name: \"shared_real\"\n}\n\
         network b {\n  external\n  name: \"shared_real\"\n}\n\
         service s {\n  image \"x\"\n  networks [a, b]\n}\n",
    );
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    let labels = &value["services"]["s"]["labels"];
    assert_eq!(
        labels,
        &serde_yaml::Value::from(vec!["traefik.docker.network=shared_real"])
    );
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

/// hl-lang#73: several entry points are several list entries, joined by
/// codegen into the one `entrypoints=` label Traefik expects — end to
/// end, through the real parse/compose/generate pipeline.
#[test]
fn several_entrypoints_join_into_one_label() {
    let yaml = generate_from(
        "service s {\n  image \"x\"\n  expose 80, host: \"ok.example.com\", entrypoint: web, web-secure\n}\n",
    );
    assert!(
        yaml.contains("traefik.http.routers.s.entrypoints=web,web-secure"),
        "expected a joined entrypoints label, got:\n{yaml}"
    );
}

/// hl-lang#73: the flip side — `entrypoint` used to be a scalar where
/// `"web,web-secure"` was the *only* way to name two entry points, so
/// this exact spelling used to compile. It's rejected now, and the
/// message says to use the list instead.
#[test]
fn comma_inside_one_entrypoint_is_error_with_a_list_hint() {
    let err = generate_err(
        "service s {\n  image \"x\"\n  expose 80, host: \"ok.example.com\", entrypoint: \"web,web-secure\"\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnsafeLabelValue {
            field: "expose.entrypoint",
            character: ',',
            ..
        }
    ));
    assert!(
        err.to_string().contains("`entrypoint` is a list"),
        "expected a list hint, got: {err}"
    );
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

/// #68: `raw` used to be flattened in on top of the built-in fields
/// without checking for collisions, so a `raw` key naming one of them
/// emitted that key *twice* in the same mapping — invalid YAML that
/// `docker compose config` rejects outright and Python's
/// `yaml.safe_load` silently reads last-wins. `raw` now wins outright
/// and the built-in is suppressed.
///
/// Note this test would fail on the old behavior at `assert_yaml_eq`'s
/// own parse step, not at the comparison: `serde_yaml` rejects a
/// duplicate mapping key.
#[test]
fn raw_key_shadowing_a_built_in_field_overrides_it() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           raw {\n    \
             image: \"override\"\n    \
             container_name: \"boom\"\n  \
           }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: override
    container_name: boom
"#,
    );
}

/// The sharp edge of override semantics, asserted deliberately: `raw`'s
/// `labels` replaces the computed Traefik labels wholesale rather than
/// merging with them. Merging would make `raw` something other than
/// verbatim passthrough.
#[test]
fn raw_labels_replace_the_computed_traefik_labels() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           expose 8080 as \"web.example.com\"\n  \
           raw {\n    labels: [\"only.this=1\"]\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
    expose:
      - 8080
    labels:
      - only.this=1
"#,
    );
}

/// Every field `ComposeServiceDoc` serializes is overridable, checked in
/// one shot — if a new built-in field is ever added without an override
/// rule, this fails (with a duplicate-key parse error) alongside the
/// exhaustive destructure in `doc.rs` that won't compile without one.
#[test]
fn every_built_in_field_is_overridable_by_raw() {
    let yaml = generate_from(
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n\
         service database {\n  image \"postgres\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           container_name \"web-ctr\"\n  \
           restart unless-stopped\n  \
           env PUID = \"1000\"\n  \
           volume \"web-data\" -> \"/data\"\n  \
           networks [traefik-net]\n  \
           dns [\"192.168.50.182\"]\n  \
           expose 8080 as \"web.example.com\"\n  \
           depends_on database\n  \
           raw {\n    \
             image: \"raw-image\"\n    \
             container_name: \"raw-name\"\n    \
             restart: \"always\"\n    \
             environment: [\"RAW=1\"]\n    \
             volumes: [\"raw-vol:/raw\"]\n    \
             networks: [\"raw-net\"]\n    \
             dns: [\"1.1.1.1\"]\n    \
             expose: [9999]\n    \
             depends_on: [\"raw-dep\"]\n    \
             labels: [\"raw.label=1\"]\n  \
           }\n\
         }\n",
    );
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{yaml}"));
    let web = &parsed["services"]["web"];
    let expected: serde_yaml::Value = serde_yaml::from_str(
        r#"
image: raw-image
container_name: raw-name
restart: always
environment:
  - RAW=1
volumes:
  - raw-vol:/raw
networks:
  - raw-net
dns:
  - 1.1.1.1
expose:
  - 9999
depends_on:
  - raw-dep
labels:
  - raw.label=1
"#,
    )
    .unwrap();
    assert_eq!(web, &expected, "\n--- actual ---\n{yaml}");
}

/// Overriding the service's own `volumes:`/`networks:` keys doesn't
/// retract the top-level `volumes:`/`networks:` declarations codegen
/// derived from the built-in fields. `raw`'s values are unparsed, so
/// there's no way to re-derive those declarations from the replacement
/// — and keeping them is what lets a `raw` value that names the same
/// named volume or network still resolve.
#[test]
fn raw_override_keeps_the_top_level_volume_and_network_declarations() {
    let yaml = generate_from(
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           volume \"web-data\" -> \"/data\"\n  \
           networks [traefik-net]\n  \
           raw {\n    \
             volumes: [\"web-data:/elsewhere\"]\n    \
             networks: [\"traefik-net\"]\n  \
           }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
    labels:
      - "traefik.docker.network=docker_default"
    volumes:
      - web-data:/elsewhere
    networks:
      - traefik-net

networks:
  traefik-net:
    name: docker_default
    external: true

volumes:
  web-data:
"#,
    );
}

/// The symmetric half of `every_built_in_field_is_overridable_by_raw`:
/// a `raw` block that names *other* keys leaves every built-in field
/// alone. Same service, same `raw` arity — only the keys differ — so
/// the two together pin the override to key equality rather than to
/// "there is a `raw` block".
#[test]
fn raw_leaves_built_in_fields_it_does_not_name_alone() {
    let yaml = generate_from(
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n\
         service database {\n  image \"postgres\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           container_name \"web-ctr\"\n  \
           restart unless-stopped\n  \
           env PUID = \"1000\"\n  \
           volume \"web-data\" -> \"/data\"\n  \
           networks [traefik-net]\n  \
           dns [\"192.168.50.182\"]\n  \
           expose 8080 as \"web.example.com\"\n  \
           depends_on database\n  \
           raw {\n    privileged: true\n    cap_add: [\"NET_ADMIN\"]\n  }\n\
         }\n",
    );
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{yaml}"));
    let web = &parsed["services"]["web"];
    let expected: serde_yaml::Value = serde_yaml::from_str(
        r#"
image: nginx
container_name: web-ctr
restart: unless-stopped
environment:
  - PUID=1000
volumes:
  - web-data:/data
networks:
  - traefik-net
dns:
  - 192.168.50.182
expose:
  - 8080
depends_on:
  - database
labels:
  - "traefik.docker.network=docker_default"
  - "traefik.http.routers.web.rule=Host(`web.example.com`)"
  - "traefik.http.services.web.loadbalancer.server.port=8080"
privileged: true
cap_add:
  - NET_ADMIN
"#,
    )
    .unwrap();
    assert_eq!(web, &expected, "\n--- actual ---\n{yaml}");
}
