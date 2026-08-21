//! Golden integration tests: generate Compose YAML from real hl-lang
//! fixtures and check it against the shape of the actual, currently
//! deployed homelab services those fixtures are modeled on. Comparisons
//! are between *parsed* YAML values, not raw strings — `serde_yaml_ng`'s
//! scalar-quoting choices don't need to match the real files
//! byte-for-byte, only semantically.

use hl_codegen::{CodegenError, CodegenWarning, generate};
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
    let a: serde_yaml_ng::Value = serde_yaml_ng::from_str(actual)
        .unwrap_or_else(|err| panic!("actual output isn't valid YAML: {err}\n{actual}"));
    let e: serde_yaml_ng::Value = serde_yaml_ng::from_str(expected).unwrap();
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

/// `cadvisor`'s host-access knobs, matching the real
/// `cadvisor/docker-compose.yml`'s `volumes`/`privileged`/`devices`/
/// `security_opt` shape exactly (not asserting label parity — the real
/// file's label has a typo, `traefiki.docker.network`, that's a bug in
/// the source data, not a codegen target). The five read-only bind
/// mounts, each written as a plain `volume "<host>" -> "<container>" {
/// read_only }` entry, come out as Compose short syntax with the `:ro`
/// suffix appended — `/:/rootfs:ro`, matching #158's own worked example
/// exactly. `privileged`/`devices` are dedicated fields (#157), and
/// `security_opt` still goes through `raw`, landing as a sibling
/// top-level service key — the genuine long tail `raw`'s job narrowed to
/// once `privileged`/`devices` graduated out of it. None of the five
/// needs `raw` any more.
#[test]
fn cadvisor_raw_passthrough_matches_real_service() {
    let yaml = generate_from(RAW_SERVICE);
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    volumes:
      - /:/rootfs:ro
      - /var/run:/var/run:ro
      - /sys:/sys:ro
      - /var/lib/docker:/var/lib/docker:ro
      - /dev/disk/:/dev/disk:ro
    privileged: true
    devices:
      - /dev/kmsg:/dev/kmsg
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
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
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
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(
        parsed["services"]["it-tools"]["container_name"],
        serde_yaml_ng::Value::String("tools".to_string())
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

/// `env_file` (#154): a single `env_file "path"` still emits Compose's
/// `env_file:` as a one-element list, the uniform shape codegen always
/// produces regardless of how many paths were written.
#[test]
fn env_file_single_path_emits_a_one_element_list() {
    let yaml = generate_from(
        "service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           env_file \"miniflux.env\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    env_file:
      - miniflux.env
"#,
    );
}

/// The list form (`env_file ["a", "b"]`) round-trips as-written, in
/// order — order matters here because Compose applies later files' env
/// vars over earlier ones when a key repeats.
#[test]
fn env_file_list_form_emits_every_path_in_order() {
    let yaml = generate_from(
        "service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           env_file [\"common.env\", \"miniflux.env\"]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    env_file:
      - common.env
      - miniflux.env
"#,
    );
}

/// `env_file` entries merge across a `with` template just like `dns`:
/// the template's own path, then the service body's, concatenated in
/// tier order.
#[test]
fn env_file_entries_merge_through_a_with_template() {
    let yaml = generate_from(
        "template with_common_env {\n  env_file \"common.env\"\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           with with_common_env\n  \
           env_file \"miniflux.env\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    env_file:
      - common.env
      - miniflux.env
"#,
    );
}

/// `raw { env_file: ... }` overrides the built-in `env_file` field, the
/// same way it overrides every other built-in field (#154).
#[test]
fn raw_env_file_overrides_the_built_in_env_file() {
    let yaml = generate_from(
        "service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           env_file \"miniflux.env\"\n  \
           raw {\n    env_file: [\"raw.env\"]\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    env_file:
      - raw.env
"#,
    );
}

// --- privileged / devices (#157) ---

/// `privileged` is bare-presence, exactly like `network`'s `external` —
/// setting it emits Compose's `privileged: true`.
#[test]
fn privileged_bare_flag_emits_true() {
    let yaml = generate_from("service cadvisor {\n  image \"nginx\"\n  privileged\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: nginx
    privileged: true
"#,
    );
}

/// Leaving `privileged` unset emits no `privileged:` key at all —
/// there's no `false` form to fall back to, since absence already means
/// false.
#[test]
fn privileged_unset_emits_no_key() {
    let yaml = generate_from("service cadvisor {\n  image \"nginx\"\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: nginx
"#,
    );
}

/// `devices ["a"]` — the bare single-item sugar every reference-list
/// field gets for free — emits Compose's `devices:` as a one-element
/// list.
#[test]
fn devices_single_entry_emits_a_one_element_list() {
    let yaml = generate_from(
        "service cadvisor {\n  image \"nginx\"\n  devices \"/dev/kmsg:/dev/kmsg\"\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: nginx
    devices:
      - /dev/kmsg:/dev/kmsg
"#,
    );
}

/// The bracketed list form round-trips every mapping in order.
#[test]
fn devices_list_form_emits_every_mapping_in_order() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"nginx\"\n  \
           devices [\"/dev/kmsg:/dev/kmsg\", \"/dev/fuse:/dev/fuse\"]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: nginx
    devices:
      - /dev/kmsg:/dev/kmsg
      - /dev/fuse:/dev/fuse
"#,
    );
}

/// `raw { privileged: ... }` / `raw { devices: ... }` override the
/// built-in fields, the same way every other built-in field does.
#[test]
fn raw_privileged_and_devices_override_the_built_in_fields() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"nginx\"\n  \
           privileged\n  \
           devices \"/dev/kmsg:/dev/kmsg\"\n  \
           raw {\n    \
             privileged: false\n    \
             devices: [\"/dev/raw:/dev/raw\"]\n  \
           }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: nginx
    privileged: false
    devices:
      - /dev/raw:/dev/raw
"#,
    );
}

// --- healthcheck (#153) ---

/// Every field set at once, shell form (`test` as a bare string).
#[test]
fn healthcheck_full_field_set_emits_every_key() {
    let yaml = generate_from(
        "service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           healthcheck {\n    \
             test: \"node /app/services/healthcheck\"\n    \
             interval: \"1m\"\n    \
             timeout: \"10s\"\n    \
             retries: 3\n    \
             start_period: \"10s\"\n    \
             start_interval: \"5s\"\n  \
           }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    healthcheck:
      test: node /app/services/healthcheck
      interval: 1m
      timeout: 10s
      retries: 3
      start_period: 10s
      start_interval: 5s
"#,
    );
}

/// The exec form: `test` as a bracketed list becomes a YAML sequence,
/// not the plain string the shell form emits.
#[test]
fn healthcheck_test_list_form_emits_a_yaml_sequence() {
    let yaml = generate_from(
        "service db {\n  \
           image \"postgres\"\n  \
           healthcheck {\n    \
             test: [\"CMD\", \"pg_isready\", \"-U\", \"miniflux\"]\n  \
           }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres
    healthcheck:
      test:
        - CMD
        - pg_isready
        - -U
        - miniflux
"#,
    );
}

/// `disable` emits Compose's `disable: true`, with no other keys when
/// nothing else was set.
#[test]
fn healthcheck_disable_emits_true() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  healthcheck { disable }\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
    healthcheck:
      disable: true
"#,
    );
}

/// A `healthcheck {}` with every sub-field left unset emits no
/// `healthcheck:` key at all — matching how a fully-unset `expose {}`
/// emits no `expose:` key.
#[test]
fn healthcheck_with_nothing_set_emits_no_key() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  healthcheck {}\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
"#,
    );
}

/// `healthcheck` sub-fields merge across a `with` template per
/// sub-field, exactly like `expose` — the template's `test` survives
/// while the service's own body overrides just `interval`.
#[test]
fn healthcheck_merges_through_a_with_template() {
    let yaml = generate_from(
        "template pg_healthcheck {\n  healthcheck { test: \"pg_isready -U miniflux\" }\n}\n\
         service db {\n  \
           image \"postgres\"\n  \
           with pg_healthcheck\n  \
           healthcheck { interval: \"10s\" }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres
    healthcheck:
      test: pg_isready -U miniflux
      interval: 10s
"#,
    );
}

/// `raw { healthcheck: ... }` overrides the built-in `healthcheck`
/// field, the same way it overrides every other built-in field.
#[test]
fn raw_healthcheck_overrides_the_built_in_healthcheck() {
    let yaml = generate_from(
        "service db {\n  \
           image \"postgres\"\n  \
           healthcheck { test: \"pg_isready\" }\n  \
           raw {\n    healthcheck: { test: \"raw-test\" }\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres
    healthcheck:
      test: raw-test
"#,
    );
}

// --- command (#156) ---
//
// The issue's own motivating case: `cadvisor.hll` needs to override the
// image's entrypoint arguments, previously only reachable through
// `raw { command: [...] }`.

/// The shell form: a bare string emits Compose's own shell-form
/// `command:` — a plain scalar, not a sequence.
#[test]
fn command_shell_form_emits_a_plain_string() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  command \"npm start\"\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
    command: npm start
"#,
    );
}

/// The exec form: a bracketed list emits a YAML sequence, exactly
/// matching the issue's own `cadvisor.hll` example — including the one
/// item with a comma embedded inside its own value
/// (`--enable_metrics=cpu,memory,network`), which has to survive as one
/// list entry rather than being split on the embedded comma.
#[test]
fn command_exec_form_emits_a_yaml_sequence() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"gcr.io/cadvisor/cadvisor:latest\"\n  \
           command [\n    \
             \"--housekeeping_interval=30s\",\n    \
             \"--docker_only=true\",\n    \
             \"--enable_metrics=cpu,memory,network\"\n  \
           ]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    command:
      - --housekeeping_interval=30s
      - --docker_only=true
      - --enable_metrics=cpu,memory,network
"#,
    );
}

/// No `command` field at all emits no `command:` key — never inferred
/// or defaulted from the image.
#[test]
fn command_unset_emits_no_key() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
"#,
    );
}

/// `command` merges like `container_name` — the service's own body
/// wins unconditionally over an inherited template value, with no
/// per-sub-field merge to consider since `command` has no sub-fields.
#[test]
fn command_merges_through_a_with_template() {
    let yaml = generate_from(
        "template base_command {\n  command \"from-template\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           with base_command\n  \
           command \"own-command\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  web:
    image: nginx
    command: own-command
"#,
    );
}

/// `raw { command: ... }` overrides the built-in `command` field, the
/// same way it overrides every other built-in field (#156) — this is
/// the escape hatch the issue's own `cadvisor.hll` example used before
/// `command` became a dedicated field.
#[test]
fn raw_command_overrides_the_built_in_command() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"gcr.io/cadvisor/cadvisor:latest\"\n  \
           command [\"--docker_only=true\"]\n  \
           raw {\n    command: [\"--raw-override=true\"]\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    command:
      - --raw-override=true
"#,
    );
}

// --- depends_on (#155) ---
//
// Every fixture below declares at least two services, since a
// `depends_on` entry has to name a real sibling — which means #152's
// multi-service auto-attach also reaches every one of them, and each
// expects its own `networks: [default]` alongside whatever `depends_on`
// itself produces.

/// The plain, unconditioned form still emits Compose's short-syntax
/// `depends_on:` — a bare list of names — exactly as it did before
/// #155, so every file written before the extended condition form
/// existed keeps compiling to the same YAML.
#[test]
fn depends_on_plain_form_emits_the_short_list_form() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           depends_on [database]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      - database
"#,
    );
}

/// An entry carrying an explicit `condition` switches the whole field to
/// Compose's long, mapping form — the two shapes can't mix in one
/// document, so a single conditioned entry is enough to commit to it.
#[test]
fn depends_on_extended_condition_emits_the_long_map_form() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           depends_on [database { condition: service_healthy }]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      database:
        condition: service_healthy
"#,
    );
}

/// A mixed list — one entry with an explicit condition, one without —
/// still emits the long form for the whole field (Compose has no way to
/// mix shapes), and the bare entry is filled in with Compose's own
/// implicit default, `service_started`, since the long form requires
/// every entry to be a mapping.
#[test]
fn depends_on_mixed_list_fills_in_the_default_condition() {
    let yaml = generate_from(
        "service cache {\n  image \"redis\"\n}\n\
         service database {\n  image \"postgres\"\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           depends_on [cache, database { condition: service_healthy }]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  cache:
    image: redis
    networks:
      - default
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      cache:
        condition: service_started
      database:
        condition: service_healthy
"#,
    );
}

/// All three of Compose's own condition values round-trip verbatim.
#[test]
fn depends_on_all_three_condition_values_round_trip() {
    let yaml = generate_from(
        "service a {\n  image \"x\"\n}\n\
         service b {\n  image \"x\"\n}\n\
         service c {\n  image \"x\"\n}\n\
         service s {\n  \
           image \"x\"\n  \
           depends_on [\n    \
             a { condition: service_started },\n    \
             b { condition: service_healthy },\n    \
             c { condition: service_completed_successfully }\n  \
           ]\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  a:
    image: x
    networks:
      - default
  b:
    image: x
    networks:
      - default
  c:
    image: x
    networks:
      - default
  s:
    image: x
    networks:
      - default
    depends_on:
      a:
        condition: service_started
      b:
        condition: service_healthy
      c:
        condition: service_completed_successfully
"#,
    );
}

/// `depends_on` merges through a `with` template just like every other
/// field — the template's own conditioned entry survives into the
/// composed service untouched.
#[test]
fn depends_on_condition_merges_through_a_with_template() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         template waits_for_db {\n  depends_on [database { condition: service_healthy }]\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           with waits_for_db\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      database:
        condition: service_healthy
"#,
    );
}

/// Two explicit `with`-listed templates that each write the same plain
/// `depends_on [database]` — no condition on either — are giving the
/// same answer twice, not two different ones, so they compose to a
/// single entry rather than colliding (see `compose.rs`'s
/// `merge_depends_on`), and the field still emits Compose's short list
/// form: nothing about composing two templates that happen to agree
/// should ever be able to flip a plain `depends_on` into the long map
/// form on its own.
#[test]
fn depends_on_identical_bare_entries_across_templates_stay_short_form() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         template a {\n  depends_on [database]\n}\n\
         template b {\n  depends_on [database]\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           with a, b\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      - database
"#,
    );
}

/// The same agreement holds when both templates spell the condition
/// out explicitly and it matches: still one entry, still no collision —
/// just the long form this time, since a `condition` was actually
/// written.
#[test]
fn depends_on_identical_explicit_conditions_across_templates_merge_to_one_entry() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         template a {\n  depends_on [database { condition: service_healthy }]\n}\n\
         template b {\n  depends_on [database { condition: service_healthy }]\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           with a, b\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      database:
        condition: service_healthy
"#,
    );
}

/// `raw { depends_on: ... }` overrides the built-in `depends_on` field,
/// the same way it overrides every other built-in field — including
/// when the built-in would otherwise have emitted the long map form.
#[test]
fn raw_depends_on_overrides_the_built_in_depends_on() {
    let yaml = generate_from(
        "service database {\n  image \"postgres\"\n}\n\
         service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           depends_on [database { condition: service_healthy }]\n  \
           raw {\n    depends_on: [\"raw-dep\"]\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  database:
    image: postgres
    networks:
      - default
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    depends_on:
      - raw-dep
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
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    let labels = &value["services"]["s"]["labels"];
    assert_eq!(
        labels,
        &serde_yaml_ng::Value::from(vec!["traefik.docker.network=shared_real"])
    );
}

// --- implicit `default` network (#152) ---

/// `default` needs no `network default {}` declaration at all: an
/// undeclared `networks [default]` resolves to Compose's own implicit
/// default network rather than raising `UnknownNetwork` — the first half
/// of #152, and true regardless of how many services the program has.
/// No top-level `networks:` entry is emitted for it either, since
/// Compose defines `default` itself.
#[test]
fn undeclared_default_network_reference_compiles() {
    let yaml = generate_from("service s {\n  image \"x\"\n  networks [default]\n}\n");
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(
        value["services"]["s"]["networks"],
        serde_yaml_ng::Value::from(vec!["default"])
    );
    assert!(
        value.get("networks").is_none(),
        "an undeclared `default` must not emit a top-level `networks:` entry: {yaml}"
    );
}

/// The auto-attach half of #152: two or more services in one program are
/// one Compose stack by construction, so every one of them lands on
/// `default` even though neither named it — with no top-level
/// `networks:` entry, exactly as the single-service case above.
#[test]
fn two_service_program_auto_attaches_default() {
    let yaml = generate_from(
        "service app {\n  image \"app\"\n}\nservice db {\n  image \"postgres:15\"\n}\n",
    );
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    for service in ["app", "db"] {
        assert_eq!(
            value["services"][service]["networks"],
            serde_yaml_ng::Value::from(vec!["default"]),
            "expected `{service}` on `default`, got: {yaml}"
        );
    }
    assert!(value.get("networks").is_none());
}

/// A lone service gets no auto-attach: Compose's own implicit default
/// network already covers a single-service project for free, so
/// emitting nothing here — no `networks:` key on the service at all —
/// is correct and matches pre-#152 output exactly.
#[test]
fn single_service_program_does_not_auto_attach() {
    let yaml = generate_from("service s {\n  image \"x\"\n}\n");
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert!(
        value["services"]["s"].get("networks").is_none(),
        "a single-service program must get no `networks:` key: {yaml}"
    );
}

/// Idempotence (#152): a service that already writes `networks
/// [default]` itself still ends up with exactly one `default` entry once
/// auto-attach runs, not two.
#[test]
fn explicit_default_reference_plus_auto_attach_is_not_duplicated() {
    let yaml = generate_from(
        "service app {\n  image \"app\"\n  networks [default]\n}\n\
         service db {\n  image \"postgres:15\"\n}\n",
    );
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(
        value["services"]["app"]["networks"],
        serde_yaml_ng::Value::from(vec!["default"])
    );
}

/// An explicit `network default { ... }` declaration still wins over the
/// implicit fallback: its `external`/`name` settings are honored exactly
/// as any other declared network's, and it still emits its own top-level
/// `networks:` entry — the implicit, doc-free `default` is only a
/// fallback for when no declaration exists at all.
#[test]
fn explicit_default_declaration_is_honored_and_emitted() {
    let yaml = generate_from(
        "network default {\n  external\n  name: \"shared_net\"\n}\n\
         service app {\n  image \"app\"\n}\nservice db {\n  image \"postgres:15\"\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  app:
    image: app
    networks: [default]
    labels:
      - "traefik.docker.network=shared_net"
  db:
    image: postgres:15
    networks: [default]
    labels:
      - "traefik.docker.network=shared_net"

networks:
  default:
    name: shared_net
    external: true
"#,
    );
}

/// #152's note on `UnusedNetwork`: auto-attach feeds `default` into the
/// same referenced-networks set the warning is checked against, so a
/// `network default {}` declared explicitly in a multi-service program
/// — now reached by every service via auto-attach — must not warn as
/// unused, even though no service names it in an explicit `networks
/// [...]` list.
#[test]
fn declared_default_in_multi_service_program_is_not_unused() {
    let program = parse(
        "network default {}\n\
         service app {\n  image \"app\"\n}\nservice db {\n  image \"postgres:15\"\n}\n",
    )
    .unwrap();
    let composed = compose(program).unwrap();
    let generated = generate(composed).unwrap();
    assert!(
        generated.warnings.is_empty(),
        "an explicitly declared `default` reached by auto-attach must not warn: {:?}",
        generated.warnings
    );
}

/// A genuinely undeclared network that isn't named `default` gets no
/// fallback and still errors — the implicit-network carve-out is
/// specific to that one name, not a general "any undeclared network is
/// fine" relaxation.
#[test]
fn undeclared_non_default_network_still_errors() {
    let err = generate_err(
        "service app {\n  image \"app\"\n  networks [proxy]\n}\n\
         service db {\n  image \"postgres:15\"\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnknownNetwork { service, network, .. }
            if service == "app" && network == "proxy"
    ));
}

// --- named volumes (#60) ---

/// A named volume's own declaration is what fills its entry in the
/// top-level `volumes:` section, exactly as a `network` declaration
/// fills its entry in `networks:` — same `external`/`name` fields, same
/// meaning, plus the two knobs only volumes have.
#[test]
fn declared_volume_options_reach_the_top_level_volumes_section() {
    let yaml = generate_from(
        "volume media {\n  external\n  name: \"media_store\"\n}\n\
         volume backups {\n  \
           driver \"local\"\n  \
           driver_opts {\n    type: \"nfs\"\n    device: \":/exports/backups\"\n  }\n\
         }\n\
         volume plain {}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin:latest\"\n  \
           volume media -> \"/data\"\n  \
           volume backups -> \"/backups\"\n  \
           volume plain -> \"/plain\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin:latest
    volumes:
      - media:/data
      - backups:/backups
      - plain:/plain

volumes:
  media:
    name: media_store
    external: true
  backups:
    driver: local
    driver_opts:
      type: nfs
      device: ":/exports/backups"
  plain:
"#,
    );
}

/// The motivating case: a typo'd (or simply undeclared) named-volume
/// reference is a hard error now, not a second, silently-created volume.
#[test]
fn undeclared_named_volume_reference_is_error() {
    let err = generate_err(
        "volume syncthing-config {}\n\
         service syncthing {\n  \
           image \"x\"\n  \
           volume snycthing-config -> \"/config\"\n\
         }\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnknownVolume { ref service, ref volume, .. }
            if service == "syncthing" && volume == "snycthing-config"
    ));
}

/// It points at the offending host literal, not at the enclosing
/// service — the same choice #70 made for `UnknownNetwork`.
#[test]
fn unknown_volume_error_points_at_the_offending_reference() {
    let err = generate_err(
        "volume known {}\n\
         service s {\n  \
           image \"x\"\n  \
           volume known -> \"/a\"\n  \
           volume nope -> \"/b\"\n\
         }\n",
    );
    let span = err.span();
    assert_eq!(
        (span.line, span.col),
        (5, 10),
        "expected the span of `\"nope\"`, got {}:{}",
        span.line,
        span.col
    );
}

/// One declaration, two services: the shared volume appears once in
/// `volumes:` and both services mount it. Before #60 this was
/// indistinguishable from two services that happened to write the same
/// string; now it's stated by referencing one declaration.
///
/// Two services also means both land on the implicit `default` network
/// (#152) — neither names one, but they're one Compose stack by
/// construction.
#[test]
fn one_volume_shared_by_two_services_is_declared_once() {
    let yaml = generate_from(
        "volume shared-media {}\n\
         service jellyfin {\n  image \"jellyfin/jellyfin\"\n  volume shared-media -> \"/data\"\n}\n\
         service sonarr {\n  image \"lscr.io/linuxserver/sonarr\"\n  volume shared-media -> \"/media\"\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin
    volumes:
      - shared-media:/data
    networks: [default]
  sonarr:
    image: lscr.io/linuxserver/sonarr
    volumes:
      - shared-media:/media
    networks: [default]

volumes:
  shared-media:
"#,
    );
}

/// Bind mounts are entirely unaffected by the declaration requirement:
/// absolute, `./`-relative and `../`-relative host paths all pass
/// straight through, and none of them puts anything in `volumes:`.
#[test]
fn bind_mount_paths_need_no_declaration() {
    let yaml = generate_from(
        "service jellyfin {\n  \
           image \"jellyfin/jellyfin\"\n  \
           volume \"/mnt/media\" -> \"/data\"\n  \
           volume \"./config\" -> \"/config\"\n  \
           volume \"../shared\" -> \"/shared\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin
    volumes:
      - /mnt/media:/data
      - ./config:/config
      - ../shared:/shared
"#,
    );
}

/// A declared volume nothing mounts isn't emitted — same as an
/// unreferenced `network` declaration.
#[test]
fn declared_but_unreferenced_volume_is_not_emitted() {
    let yaml = generate_from("volume unused {}\nservice s {\n  image \"x\"\n}\n");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert!(
        parsed.get("volumes").is_none(),
        "expected no top-level volumes section, got:\n{yaml}"
    );
}

/// A *quoted* host side is a bind-mount path whatever its content, with
/// no leading `/` or `.` required and no declaration looked for — the
/// distinction is syntactic now, not a guess at the string's shape. So
/// `"media"` is a path, and the same word unquoted would be a reference.
#[test]
fn a_quoted_host_is_a_bind_mount_whatever_it_says() {
    let yaml = generate_from("service s {\n  image \"x\"\n  volume \"media\" -> \"/data\"\n}\n");
    assert_yaml_eq(
        &yaml,
        r#"
services:
  s:
    image: x
    volumes:
      - media:/data
"#,
    );
}

/// A bind-mount path is an ordinary value, so `{{name}}` interpolates
/// into it like anywhere else. (A named-volume *reference* has no
/// interpolated form: it's an identifier resolved against a declaration,
/// exactly like a `networks [x]` entry.)
#[test]
fn interpolation_reaches_a_bind_mount_path() {
    let yaml = generate_from(
        "service syncthing {\n  image \"x\"\n  volume \"/srv/{{name}}\" -> \"/config\"\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  syncthing:
    image: x
    volumes:
      - /srv/syncthing:/config
"#,
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

/// #84: `publish` becomes Compose's `ports:` — a host-published port,
/// the thing `expose:` deliberately isn't. Modeled on Pi-hole, the
/// issue's own motivating case: a service reached directly on the LAN
/// rather than through Traefik, on both protocols of one host port plus
/// an admin UI on a remapped one.
///
/// `expose` is untouched by this: the service below sets both, and each
/// lands in its own Compose key with its own meaning.
#[test]
fn publish_becomes_the_compose_ports_list() {
    let yaml = generate_from(
        "service pihole {\n  \
           image \"pihole/pihole:latest\"\n  \
           publish 53 -> \"53/tcp\"\n  \
           publish 53 -> \"53/udp\"\n  \
           publish 8081 -> 80\n  \
           expose 80\n  \
           restart unless-stopped\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  pihole:
    image: pihole/pihole:latest
    restart: unless-stopped
    ports:
      - "53:53/tcp"
      - "53:53/udp"
      - "8081:80"
    expose:
      - 80
"#,
    );
}

/// A `publish` entry inherited from a template resolves its `{{name}}`
/// interpolation and `$param` substitution like every other value slot,
/// rather than being passed through verbatim.
#[test]
fn publish_entries_from_a_template_are_fully_resolved() {
    let yaml = generate_from(
        "template published(port: Number) {\n  publish $port -> $port\n}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin:latest\"\n  \
           with published { port: 8096 }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin:latest
    ports:
      - "8096:8096"
"#,
    );
}

/// #84's `publish` and #60's top-level `volume` declaration are
/// unrelated features that landed in the same release, so one service
/// using both at once is the case neither one's own tests cover: the
/// host-published port lands in `ports:` and the declared named volume
/// still reaches both the service's `volumes:` list and the document's
/// top-level `volumes:` section, each carrying its own declaration's
/// settings.
#[test]
fn publish_and_a_declared_named_volume_compose_together() {
    let yaml = generate_from(
        "volume syncthing-config {\n  driver \"local\"\n}\n\
         service syncthing {\n  \
           image \"lscr.io/linuxserver/syncthing:latest\"\n  \
           publish 8384 -> 8384\n  \
           publish 22000 -> \"22000/tcp\"\n  \
           volume syncthing-config -> \"/config\"\n  \
           volume \"/mnt/media\" -> \"/data\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  syncthing:
    image: lscr.io/linuxserver/syncthing:latest
    volumes:
      - syncthing-config:/config
      - /mnt/media:/data
    ports:
      - "8384:8384"
      - "22000:22000/tcp"

volumes:
  syncthing-config:
    driver: local
"#,
    );
}

/// #68: `raw` used to be flattened in on top of the built-in fields
/// without checking for collisions, so a `raw` key naming one of them
/// emitted that key *twice* in the same mapping — invalid YAML that
/// `docker compose config` rejects outright and Python's
/// `yaml.safe_load` silently reads last-wins. `raw` now wins outright
/// and the built-in is suppressed.
///
/// Note this test would fail on the old behavior at `assert_yaml_eq`'s
/// own parse step, not at the comparison: `serde_yaml_ng` rejects a
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
         volume web-data {}\n\
         service database {\n  image \"postgres\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           container_name \"web-ctr\"\n  \
           command \"npm start\"\n  \
           privileged\n  \
           restart unless-stopped\n  \
           healthcheck { test: \"curl -f http://localhost\" }\n  \
           env PUID = \"1000\"\n  \
           env_file \"web.env\"\n  \
           volume web-data -> \"/data\"\n  \
           networks [traefik-net]\n  \
           dns [\"192.168.50.182\"]\n  \
           devices [\"/dev/original:/dev/original\"]\n  \
           publish 8080 -> 8080\n  \
           expose 8080 as \"web.example.com\"\n  \
           depends_on database\n  \
           raw {\n    \
             image: \"raw-image\"\n    \
             container_name: \"raw-name\"\n    \
             command: [\"raw-command\"]\n    \
             privileged: false\n    \
             restart: \"always\"\n    \
             healthcheck: { test: \"raw-test\" }\n    \
             environment: [\"RAW=1\"]\n    \
             env_file: [\"raw.env\"]\n    \
             volumes: [\"raw-vol:/raw\"]\n    \
             networks: [\"raw-net\"]\n    \
             dns: [\"1.1.1.1\"]\n    \
             devices: [\"/dev/raw:/dev/raw\"]\n    \
             ports: [\"7777:7777\"]\n    \
             expose: [9999]\n    \
             depends_on: [\"raw-dep\"]\n    \
             labels: [\"raw.label=1\"]\n  \
           }\n\
         }\n",
    );
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{yaml}"));
    let web = &parsed["services"]["web"];
    let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
image: raw-image
container_name: raw-name
command:
  - raw-command
privileged: false
restart: always
healthcheck:
  test: raw-test
environment:
  - RAW=1
env_file:
  - raw.env
volumes:
  - raw-vol:/raw
networks:
  - raw-net
dns:
  - 1.1.1.1
devices:
  - /dev/raw:/dev/raw
ports:
  - "7777:7777"
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
         volume web-data {}\n\
         service web {\n  \
           image \"nginx\"\n  \
           volume web-data -> \"/data\"\n  \
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
///
/// `web`'s `networks:` list ends with `default` alongside its explicit
/// `traefik-net` — this fixture declares two services, so #152's
/// auto-attach reaches it too.
#[test]
fn raw_leaves_built_in_fields_it_does_not_name_alone() {
    let yaml = generate_from(
        "network traefik-net {\n  external\n  name: \"docker_default\"\n}\n\
         volume web-data {}\n\
         service database {\n  image \"postgres\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           container_name \"web-ctr\"\n  \
           command \"npm start\"\n  \
           restart unless-stopped\n  \
           healthcheck { test: \"curl -f http://localhost\" }\n  \
           env PUID = \"1000\"\n  \
           env_file \"web.env\"\n  \
           volume web-data -> \"/data\"\n  \
           networks [traefik-net]\n  \
           dns [\"192.168.50.182\"]\n  \
           publish 8080 -> 8080\n  \
           expose 8080 as \"web.example.com\"\n  \
           depends_on database\n  \
           raw {\n    privileged: true\n    cap_add: [\"NET_ADMIN\"]\n  }\n\
         }\n",
    );
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{yaml}"));
    let web = &parsed["services"]["web"];
    let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
image: nginx
container_name: web-ctr
command: npm start
restart: unless-stopped
healthcheck:
  test: curl -f http://localhost
environment:
  - PUID=1000
env_file:
  - web.env
volumes:
  - web-data:/data
networks:
  - traefik-net
  - default
dns:
  - 192.168.50.182
ports:
  - "8080:8080"
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

/// Values that look like YAML structure — `: `, a leading/embedded `#`,
/// a NUL byte, flow/indicator characters — have to survive the round
/// trip *as data*, on every channel a user-supplied string can reach:
/// `container_name`, `env` values, volume paths, an external network's
/// or volume's real `name` (the network's also lands inside a Traefik
/// label), and both keys and values inside `raw`. If the serializer ever emitted one of these
/// unquoted, the reparse below wouldn't just differ — it would come back
/// with a different *shape* (an extra nested mapping, a truncated value
/// where a `#` started a comment), which is exactly the YAML-structure
/// injection this pins shut.
///
/// This is the invariant #126 had to preserve when the workspace moved
/// off the deprecated `serde_yaml` to `serde_yaml_ng`, so it's asserted
/// here rather than left implicit in the fixture-shaped tests above —
/// it's the property that has to hold across *any* future swap of the
/// underlying YAML library, not just that one.
///
/// A literal newline isn't covered because it can't reach codegen: the
/// lexer rejects a raw newline inside a string literal
/// (`LexError::UnterminatedString`) and there are no escape sequences,
/// so no `.hll` source can express one.
#[test]
fn yaml_hostile_values_round_trip_as_data_not_structure() {
    let yaml = generate_from(
        "network shared {\n  \
           external\n  \
           name: \"net: with # hash\"\n\
         }\n\
         volume hostile-vol {\n  \
           name: \"vol: name # hash\"\n\
         }\n\
         service web {\n  \
           image \"nginx\"\n  \
           container_name \"ctr: name # here\"\n  \
           env COLON = \"value: with colon\"\n  \
           env HASH = \"value # with hash\"\n  \
           env NUL = \"value\0with nul\"\n  \
           env FLOW = \"[a, b]{c: d}\"\n  \
           env ANCHOR = \"&anchor *alias\"\n  \
           volume hostile-vol -> \"/mnt/# hash\"\n  \
           networks [shared]\n  \
           raw {\n    \
             colon_val: \"raw: colon value\"\n    \
             hash_val: \"raw # hash value\"\n    \
             nul_val: \"raw\0nul\"\n    \
             nested: {\n      \
               \"key: with colon\": \"v1\"\n      \
               \"key # with hash\": \"v2\"\n    \
             }\n  \
           }\n\
         }\n",
    );
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{yaml}"));
    let web = &parsed["services"]["web"];

    /// Shorthand for the YAML string a value is expected to come back as.
    fn s(text: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::Value::String(text.to_string())
    }

    assert_eq!(web["container_name"], s("ctr: name # here"));

    let env: Vec<&str> = web["environment"]
        .as_sequence()
        .expect("environment is a sequence")
        .iter()
        .map(|v| v.as_str().expect("env entry is a string"))
        .collect();
    assert_eq!(
        env,
        vec![
            "COLON=value: with colon",
            "HASH=value # with hash",
            "NUL=value\0with nul",
            "FLOW=[a, b]{c: d}",
            "ANCHOR=&anchor *alias",
        ],
        "\n--- actual ---\n{yaml}"
    );

    assert_eq!(
        web["volumes"],
        serde_yaml_ng::Value::Sequence(vec![s("hostile-vol:/mnt/# hash")]),
        "\n--- actual ---\n{yaml}"
    );
    assert_eq!(web["colon_val"], s("raw: colon value"));
    assert_eq!(web["hash_val"], s("raw # hash value"));
    assert_eq!(web["nul_val"], s("raw\0nul"));
    assert_eq!(web["nested"]["key: with colon"], s("v1"));
    assert_eq!(web["nested"]["key # with hash"], s("v2"));

    // The external network's real name reaches the output twice: once as
    // `networks.shared.name`, once inside the computed Traefik label.
    assert_eq!(parsed["networks"]["shared"]["name"], s("net: with # hash"));
    assert!(
        web["labels"]
            .as_sequence()
            .expect("labels is a sequence")
            .contains(&s("traefik.docker.network=net: with # hash")),
        "\n--- actual ---\n{yaml}"
    );

    // A named volume's own key is an `.hll` identifier since #60 made
    // the top-level declaration mandatory, so the hostile characters now
    // reach the output through its `name:` override instead — the exact
    // same channel as the network's, and a `#` there would otherwise
    // comment out the rest of the mapping.
    assert_eq!(
        parsed["volumes"]["hostile-vol"]["name"],
        s("vol: name # hash"),
        "\n--- actual ---\n{yaml}"
    );
}

/// #80: `middleware` with no `expose.host` used to compile to a service
/// with no `labels:` key at all — the auth middleware the user asked for
/// silently absent from a deployment that otherwise looked fine. There
/// is no router for it to attach to, so this is a build failure now.
#[test]
fn middleware_without_a_host_is_an_error() {
    let err = generate_err("service w {\n  image \"n\"\n  expose 80\n  middleware auth\n}\n");
    assert!(
        matches!(
            &err,
            CodegenError::RouterFieldWithoutHost {
                service,
                field: "middleware",
                ..
            } if service == "w"
        ),
        "expected a router-less middleware error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "4:14: service `w` sets `middleware` but has no `expose.host`, so there is no Traefik \
         router to attach it to — add a host (`expose <port> as \"w.example.com\"`) or drop the \
         `middleware`"
    );
}

/// The same rule for the other router-attached field, and the one
/// reported first when a service sets both — a host-less `expose` block
/// is exactly the shape a forgotten `as "..."` leaves behind.
#[test]
fn entrypoint_without_a_host_is_an_error() {
    let err = generate_err(
        "service w {\n  image \"n\"\n  expose 80, entrypoint: web-secure\n  middleware auth\n}\n",
    );
    assert!(
        matches!(
            err,
            CodegenError::RouterFieldWithoutHost {
                field: "expose.entrypoint",
                ..
            }
        ),
        "expected the entrypoint to be reported first, got: {err:?}"
    );
}

/// A service that sets neither router-attached field is unaffected — no
/// host, no router, no labels, and no diagnostic.
#[test]
fn a_hostless_service_without_router_fields_still_builds() {
    let yaml = generate_from("service w {\n  image \"n\"\n  expose 80\n}\n");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    assert!(parsed["services"]["w"].get("labels").is_none());
}

/// #80: `networks:` is assembled from what services reference, so a
/// declaration nothing references never reaches the output. That stays
/// true — it's a warning, and the build still succeeds.
#[test]
fn an_unreferenced_network_warns_but_still_builds() {
    let program =
        parse("network unused {\n  external\n}\nservice w {\n  image \"n\"\n}\n").unwrap();
    let composed = compose(program).unwrap();
    let generated = generate(composed).expect("an unused network is not an error");

    assert!(
        matches!(
            generated.warnings.as_slice(),
            [CodegenWarning::UnusedNetwork { network, .. }] if network == "unused"
        ),
        "expected one unused-network warning, got: {:?}",
        generated.warnings
    );
    assert_eq!(
        generated.warnings[0].to_string(),
        "1:9: warning: network `unused` is declared but no service references it, so it is not \
         emitted — add it to a service's `networks [...]` list, or remove the declaration"
    );
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&generated.yaml).unwrap();
    assert!(parsed.get("networks").is_none());
}

/// A network a service actually names is emitted, and says nothing.
#[test]
fn a_referenced_network_produces_no_warning() {
    let program = parse(
        "network proxy {\n  external\n}\nservice w {\n  image \"n\"\n  networks [proxy]\n}\n",
    )
    .unwrap();
    let composed = compose(program).unwrap();
    let generated = generate(composed).unwrap();
    assert!(
        generated.warnings.is_empty(),
        "unexpected warnings: {:?}",
        generated.warnings
    );
}

/// #158's `{ read_only }` flag on a *named* Docker volume, not just a
/// bind mount — the cadvisor fixture above only exercises the
/// bind-mount side, and the two go through different `VolumeHost` arms
/// in `resolve_volumes`, so this is the named-volume half of "must work
/// for both." Mixes a flagged entry with an unflagged one in the same
/// service, which is what actually exercises both of `resolve_volumes`'s
/// `:ro`-or-not branches in one generated document (and is the surface a
/// missed mutant on the `if v.read_only` check would show up on: flip it
/// and either this entry loses its suffix or the other one gains one it
/// shouldn't have).
#[test]
fn named_volume_read_only_flag_emits_ro_suffix_alongside_an_unflagged_entry() {
    let yaml = generate_from(
        "volume media {}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin:latest\"\n  \
           volume media -> \"/data\" { read_only }\n  \
           volume \"/mnt/config\" -> \"/config\"\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  jellyfin:
    image: jellyfin/jellyfin:latest
    volumes:
      - media:/data:ro
      - /mnt/config:/config

volumes:
  media:
"#,
    );
}

// --- `traefik { disabled }` (#159) ---

/// The issue's own worked example, end to end: `miniflux`'s `db` backing
/// service opts out of every Traefik label with `traefik { disabled }`
/// instead of replacing the whole computed `labels:` list through `raw`.
/// `miniflux` itself is an ordinary Traefik-facing service, unaffected —
/// this is the "one backend-only service in an otherwise
/// Traefik-facing stack" shape #159 names directly.
#[test]
fn miniflux_db_disables_traefik_end_to_end() {
    let yaml = generate_from(
        "service miniflux {\n  \
           image \"miniflux/miniflux:latest\"\n  \
           expose 8080 as \"miniflux.example.com\"\n  \
           depends_on db\n\
         }\n\
         service db {\n  \
           image \"postgres:15\"\n  \
           traefik {\n    disabled\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  miniflux:
    image: miniflux/miniflux:latest
    networks:
      - default
    expose:
      - 8080
    depends_on:
      - db
    labels:
      - "traefik.http.routers.miniflux.rule=Host(`miniflux.example.com`)"
      - "traefik.http.services.miniflux.loadbalancer.server.port=8080"

  db:
    image: postgres:15
    networks:
      - default
    labels:
      - "traefik.enable=false"
"#,
    );
}

/// Without `docker_network`/`expose.host`/`middleware` in the way, a
/// disabled service's label list really is exactly the one line —
/// checked directly against the raw string, not just parsed-YAML
/// equality, since "exactly one label and nothing else" is precisely
/// the guarantee at stake.
#[test]
fn disabled_service_emits_exactly_one_label() {
    let yaml = generate_from("service db {\n  image \"postgres:15\"\n  traefik { disabled }\n}\n");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    let labels = parsed["services"]["db"]["labels"]
        .as_sequence()
        .expect("labels is a sequence");
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].as_str().unwrap(), "traefik.enable=false");
}

/// A service that never writes `traefik` at all is byte-for-byte
/// unaffected by this field existing — the exact same assertion
/// `syncthing_matches_real_deployed_service` already makes, re-run here
/// to pin down that adding `traefik` to the schema changed nothing about
/// a program that doesn't use it.
#[test]
fn a_service_without_traefik_field_is_unaffected() {
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

/// `expose.port` alone doesn't conflict with `disabled` — it's Compose's
/// own `expose:` key, plain container-network visibility, nothing to do
/// with Traefik. `db`'s own `expose 5432` from the issue's real shape.
#[test]
fn disabled_service_may_still_declare_expose_port() {
    let yaml = generate_from(
        "service db {\n  image \"postgres:15\"\n  expose 5432\n  traefik { disabled }\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres:15
    expose:
      - 5432
    labels:
      - "traefik.enable=false"
"#,
    );
}

#[test]
fn traefik_disabled_with_expose_host_is_an_error() {
    let err = generate_err(
        "service db {\n  image \"postgres:15\"\n  expose 5432 as \"db.example.com\"\n  traefik { disabled }\n}\n",
    );
    assert!(
        matches!(
            &err,
            CodegenError::TraefikDisabledWithRouterField {
                service,
                field: "expose.host",
                ..
            } if service == "db"
        ),
        "got {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "3:18: service `db` sets `expose.host`, but `traefik` is disabled (at 4:13), so there is \
         no router for it to attach to — drop the `expose.host` or remove `disabled`"
    );
}

#[test]
fn traefik_disabled_with_middleware_is_an_error() {
    let err = generate_err(
        "service db {\n  image \"postgres:15\"\n  middleware auth\n  traefik { disabled }\n}\n",
    );
    assert!(
        matches!(
            err,
            CodegenError::TraefikDisabledWithRouterField {
                field: "middleware",
                ..
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn traefik_disabled_with_entrypoint_is_an_error() {
    let err = generate_err(
        "service db {\n  \
           image \"postgres:15\"\n  \
           expose 5432, entrypoint: web-secure\n  \
           traefik { disabled }\n\
         }\n",
    );
    assert!(
        matches!(
            err,
            CodegenError::TraefikDisabledWithRouterField {
                field: "expose.entrypoint",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// `raw { labels: [...] }` still overrides the computed list entirely —
/// including a disabled service's single `traefik.enable=false` line —
/// exactly as it overrides the ordinary computed router labels.
#[test]
fn raw_labels_override_a_disabled_services_label_too() {
    let yaml = generate_from(
        "service db {\n  \
           image \"postgres:15\"\n  \
           traefik { disabled }\n  \
           raw {\n    labels: [\"only.this=1\"]\n  }\n\
         }\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres:15
    labels:
      - only.this=1
"#,
    );
}

/// `traefik { disabled }` composes through `with` just like any other
/// nested field — a template can carry the "no Traefik" shape for every
/// backend-only service that reuses it.
#[test]
fn traefik_disabled_composes_through_a_template() {
    let yaml = generate_from(
        "template backend_only {\n  traefik { disabled }\n}\n\
         service db {\n  with backend_only\n  image \"postgres:15\"\n}\n",
    );
    assert_yaml_eq(
        &yaml,
        r#"
services:
  db:
    image: postgres:15
    labels:
      - "traefik.enable=false"
"#,
    );
}
