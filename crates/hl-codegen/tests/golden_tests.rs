//! Golden integration tests: generate Compose YAML from real hl-lang
//! fixtures and check it against the shape of the actual, currently
//! deployed homelab services those fixtures are modeled on. Comparisons
//! are between *parsed* YAML values, not raw strings — `serde_yaml_ng`'s
//! scalar-quoting choices don't need to match the real files
//! byte-for-byte, only semantically.
//!
//! `insta` is what performs that comparison. Every expectation is an
//! inline snapshot sitting next to the test that produced it, taken of
//! the parsed `serde_yaml_ng::Value` rather than of the rendered text,
//! so a change in how the serializer quotes a scalar still compares
//! equal. After a deliberate codegen change, `cargo insta review` walks
//! the pending diffs one at a time — reading each one is the point,
//! because accepting a snapshot claims the new output is correct.

use hl_codegen::{CodegenError, CodegenWarning, generate};
use hl_parser::{compose, parse};
use insta::assert_yaml_snapshot;

const RAW_SERVICE: &str = include_str!("../../hl-parser/tests/fixtures/raw_service.hll");

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

fn yaml_value(rendered: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::from_str(rendered)
        .unwrap_or_else(|err| panic!("output isn't valid YAML: {err}\n{rendered}"))
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
/// needs `raw` any more. That `raw` body writes `security_opt` as the
/// list of `option:value` strings Compose's schema asks for (#174):
/// nothing validates a `raw` value on the way through, so the shape
/// Compose wants has to be the shape the author writes.
#[test]
fn cadvisor_raw_passthrough_matches_real_service() {
    let yaml = generate_from(RAW_SERVICE);
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: "gcr.io/cadvisor/cadvisor:latest"
        privileged: true
        volumes:
          - "/:/rootfs:ro"
          - "/var/run:/var/run:ro"
          - "/sys:/sys:ro"
          - "/var/lib/docker:/var/lib/docker:ro"
          - "/dev/disk/:/dev/disk:ro"
        devices:
          - "/dev/kmsg:/dev/kmsg"
        security_opt:
          - "seccomp:unconfined"
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      uptime-kuma:
        image: "louislam/uptime-kuma:latest"
        dns:
          - 192.168.50.182
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      miniflux:
        image: "miniflux/miniflux:latest"
        env_file:
          - miniflux.env
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      miniflux:
        image: "miniflux/miniflux:latest"
        env_file:
          - common.env
          - miniflux.env
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      miniflux:
        image: "miniflux/miniflux:latest"
        env_file:
          - common.env
          - miniflux.env
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      miniflux:
        image: "miniflux/miniflux:latest"
        env_file:
          - raw.env
    "#);
}

// --- privileged / devices (#157) ---

/// `privileged` is bare-presence, exactly like `network`'s `external` —
/// setting it emits Compose's `privileged: true`.
#[test]
fn privileged_bare_flag_emits_true() {
    let yaml = generate_from("service cadvisor {\n  image \"nginx\"\n  privileged\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      cadvisor:
        image: nginx
        privileged: true
    ");
}

/// Leaving `privileged` unset emits no `privileged:` key at all —
/// there's no `false` form to fall back to, since absence already means
/// false.
#[test]
fn privileged_unset_emits_no_key() {
    let yaml = generate_from("service cadvisor {\n  image \"nginx\"\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      cadvisor:
        image: nginx
    ");
}

/// `devices "/dev/kmsg" -> "/dev/kmsg"` — the arrow bare-entry sugar
/// every map-kind field gets, mirroring `publish`'s own syntax per
/// #167's review feedback — emits Compose's `devices:` as a one-element
/// `"host:container"` list.
#[test]
fn devices_single_entry_emits_a_one_element_list() {
    let yaml = generate_from(
        "service cadvisor {\n  image \"nginx\"\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: nginx
        devices:
          - "/dev/kmsg:/dev/kmsg"
    "#);
}

/// The canonical multi-entry `{ }` body round-trips every mapping in
/// order, exactly like `publish`'s own canonical body.
#[test]
fn devices_canonical_body_emits_every_mapping_in_order() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"nginx\"\n  \
           devices { \"/dev/kmsg\": \"/dev/kmsg\", \"/dev/fuse\": \"/dev/fuse\" }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: nginx
        devices:
          - "/dev/kmsg:/dev/kmsg"
          - "/dev/fuse:/dev/fuse"
    "#);
}

/// A quoted container side carries Compose's optional cgroup
/// permissions suffix through untouched, exactly like `publish`'s own
/// protocol suffix.
#[test]
fn devices_container_side_permissions_suffix_rides_through() {
    let yaml = generate_from(
        "service cadvisor {\n  image \"nginx\"\n  devices \"/dev/sda\" -> \"/dev/xvda:rwm\"\n}\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: nginx
        devices:
          - "/dev/sda:/dev/xvda:rwm"
    "#);
}

/// `raw { privileged: ... }` / `raw { devices: ... }` override the
/// built-in fields, the same way every other built-in field does.
#[test]
fn raw_privileged_and_devices_override_the_built_in_fields() {
    let yaml = generate_from(
        "service cadvisor {\n  \
           image \"nginx\"\n  \
           privileged\n  \
           devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n  \
           raw {\n    \
             privileged: false\n    \
             devices: [\"/dev/raw:/dev/raw\"]\n  \
           }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: nginx
        privileged: false
        devices:
          - "/dev/raw:/dev/raw"
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      miniflux:
        image: "miniflux/miniflux:latest"
        healthcheck:
          test: node /app/services/healthcheck
          interval: 1m
          timeout: 10s
          retries: 3
          start_period: 10s
          start_interval: 5s
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      db:
        image: postgres
        healthcheck:
          test:
            - CMD
            - pg_isready
            - "-U"
            - miniflux
    "#);
}

/// `disable` emits Compose's `disable: true`, with no other keys when
/// nothing else was set.
#[test]
fn healthcheck_disable_emits_true() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  healthcheck { disable }\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        healthcheck:
          disable: true
    ");
}

/// A `healthcheck {}` with every sub-field left unset emits no
/// `healthcheck:` key at all — matching how a fully-unset `expose {}`
/// emits no `expose:` key.
#[test]
fn healthcheck_with_nothing_set_emits_no_key() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  healthcheck {}\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      db:
        image: postgres
        healthcheck:
          test: pg_isready -U miniflux
          interval: 10s
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      db:
        image: postgres
        healthcheck:
          test: raw-test
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        command: npm start
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: "gcr.io/cadvisor/cadvisor:latest"
        command:
          - "--housekeeping_interval=30s"
          - "--docker_only=true"
          - "--enable_metrics=cpu,memory,network"
    "#);
}

/// No `command` field at all emits no `command:` key — never inferred
/// or defaulted from the image.
#[test]
fn command_unset_emits_no_key() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        command: own-command
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      cadvisor:
        image: "gcr.io/cadvisor/cadvisor:latest"
        command:
          - "--raw-override=true"
    "#);
}

// --- entrypoint (#183) ---
//
// Compose's `entrypoint:` key, which overrides the image's own
// `ENTRYPOINT` where `command` above overrides its `CMD`. Previously
// only reachable through `raw { entrypoint: ... }`.

/// The shell form: a bare string emits Compose's own shell-form
/// `entrypoint:` — a plain scalar, not a sequence.
#[test]
fn entrypoint_shell_form_emits_a_plain_string() {
    let yaml = generate_from(
        "service web {\n  image \"nginx\"\n  entrypoint \"/bin/sh -c 'do-a-thing'\"\n}\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      web:
        image: nginx
        entrypoint: "/bin/sh -c 'do-a-thing'"
    "#);
}

/// The exec form: a bracketed list emits a YAML sequence, the issue's
/// second spelling (#183).
#[test]
fn entrypoint_exec_form_emits_a_yaml_sequence() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           entrypoint [\"/bin/sh\", \"-c\", \"do-a-thing\"]\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      web:
        image: nginx
        entrypoint:
          - /bin/sh
          - "-c"
          - do-a-thing
    "#);
}

/// No `entrypoint` field at all emits no `entrypoint:` key — never
/// inferred or defaulted from the image.
#[test]
fn entrypoint_unset_emits_no_key() {
    let yaml = generate_from("service web {\n  image \"nginx\"\n  command \"npm start\"\n}\n");
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        command: npm start
    ");
}

/// `entrypoint` and `command` are two different Compose keys, so a
/// service setting both emits both — `entrypoint:` first, the order the
/// two halves take in the container's own argument vector.
#[test]
fn entrypoint_and_command_are_emitted_as_separate_keys() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           entrypoint [\"/bin/sh\", \"-c\"]\n  \
           command \"do-a-thing\"\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      web:
        image: nginx
        entrypoint:
          - /bin/sh
          - "-c"
        command: do-a-thing
    "#);
}

/// `entrypoint` merges like `command` — the service's own body wins
/// unconditionally over an inherited template value, with no
/// per-sub-field merge to consider since `entrypoint` has no
/// sub-fields.
#[test]
fn entrypoint_merges_through_a_with_template() {
    let yaml = generate_from(
        "template base_entrypoint {\n  entrypoint \"/from-template.sh\"\n}\n\
         service web {\n  \
           image \"nginx\"\n  \
           with base_entrypoint\n  \
           entrypoint \"/own.sh\"\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        entrypoint: /own.sh
    ");
}

/// `raw { entrypoint: ... }` overrides the built-in `entrypoint` field,
/// the same way it overrides every other built-in field — this is the
/// escape hatch the issue reports having to use before the field
/// existed.
#[test]
fn raw_entrypoint_overrides_the_built_in_entrypoint() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           entrypoint \"/built-in.sh\"\n  \
           raw {\n    entrypoint: [\"/raw-override.sh\"]\n  }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        entrypoint:
          - /raw-override.sh
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          - database
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          database:
            condition: service_healthy
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
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
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          cache:
            condition: service_started
          database:
            condition: service_healthy
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
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
    ");
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          database:
            condition: service_healthy
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          - database
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          database:
            condition: service_healthy
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      database:
        image: postgres
        networks:
          - default
      miniflux:
        image: "miniflux/miniflux:latest"
        networks:
          - default
        depends_on:
          - raw-dep
    "#);
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

// --- #196: `$param` reaches every reference-shaped position ---

/// The reproduction #196 set out to fix: `networks` was `Reference`-typed
/// before the `Literal`/`Reference` unification, and a `Reference` had
/// nowhere to put a `$param` — `networks [$net]` was a parse error with
/// no way to fix it. This is the positive case: a real declared network
/// resolves correctly once the parameter is bound.
#[test]
fn parameterized_network_resolves_to_the_bound_argument() {
    let yaml = generate_from(
        "network proxy {\n  name: \"real_proxy\"\n}\n\
         template web(net) {\n  networks [$net]\n}\n\
         service app {\n  image \"nginx\"\n  with web { net: \"proxy\" }\n}\n",
    );
    let value = yaml_value(&yaml);
    assert_eq!(
        value["services"]["app"]["networks"],
        serde_yaml_ng::Value::from(vec!["proxy"])
    );
    assert_eq!(value["networks"]["proxy"]["name"], "real_proxy");
}

/// Hard constraint (#196): a `$param` substituted into `networks` must
/// still resolve by name at codegen — the parser accepting `$net`
/// syntactically must never let a name that resolves to nothing declared
/// bypass `UnknownNetwork`. This is the single most likely way to get
/// the unification wrong (a `Literal::Param` slot skipping the same
/// by-name check every other `networks` entry goes through), so it gets
/// its own hand-written assertion on the exact error variant, not just
/// the rendered diagnostic text `tests/cases/` pins.
#[test]
fn parameterized_network_naming_something_undeclared_is_still_unknown_network() {
    let err = generate_err(
        "template web(net) {\n  networks [$net]\n}\n\
         service app {\n  image \"nginx\"\n  with web { net: \"ghost\" }\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnknownNetwork { service, network, .. }
            if service == "app" && network == "ghost"
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: "jellyfin/jellyfin:latest"
        volumes:
          - "media:/data"
          - "backups:/backups"
          - "plain:/plain"
    volumes:
      media:
        name: media_store
        external: true
      backups:
        driver: local
        driver_opts:
          type: nfs
          device: ":/exports/backups"
      plain: ~
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: jellyfin/jellyfin
        volumes:
          - "shared-media:/data"
        networks:
          - default
      sonarr:
        image: lscr.io/linuxserver/sonarr
        volumes:
          - "shared-media:/media"
        networks:
          - default
    volumes:
      shared-media: ~
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: jellyfin/jellyfin
        volumes:
          - "/mnt/media:/data"
          - "./config:/config"
          - "../shared:/shared"
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      s:
        image: x
        volumes:
          - "media:/data"
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      syncthing:
        image: x
        volumes:
          - "/srv/syncthing:/config"
    "#);
}

#[test]
fn missing_image_and_build_is_error() {
    let err = generate_err("service s {\n}\n");
    assert!(matches!(
        err,
        CodegenError::MissingImageOrBuild { service, .. } if service == "s"
    ));
}

#[test]
fn unknown_interpolation_is_error() {
    let err = generate_err(
        "service s {\n  image \"x\"\n  labels { \"k\": \"{{typo}}.example.com\" }\n}\n",
    );
    assert!(matches!(
        err,
        CodegenError::UnknownInterpolation { binding, .. } if binding == "typo"
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      pihole:
        image: "pihole/pihole:latest"
        restart: unless-stopped
        ports:
          - "53:53/tcp"
          - "53:53/udp"
          - "8081:80"
        expose:
          - 80
    "#);
}

/// A `publish` entry inherited from a template resolves its `{{name}}`
/// interpolation and `$param` substitution like every other value slot,
/// rather than being passed through verbatim.
#[test]
fn publish_entries_from_a_template_are_fully_resolved() {
    let yaml = generate_from(
        "template published(port) {\n  publish $port -> $port\n}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin:latest\"\n  \
           with published { port: 8096 }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: "jellyfin/jellyfin:latest"
        ports:
          - "8096:8096"
    "#);
}

/// `devices` (#167) resolves `$param` substitution on both sides of an
/// inherited mapping exactly like `publish`'s own entries just above —
/// the same live bug class issue #168 covers, guarding against a
/// `$param` surviving composition unresolved.
#[test]
fn devices_entries_from_a_template_are_fully_resolved() {
    let yaml = generate_from(
        "template gpu(dev) {\n  devices $dev -> $dev\n}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin:latest\"\n  \
           with gpu { dev: \"/dev/dri\" }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: "jellyfin/jellyfin:latest"
        devices:
          - "/dev/dri:/dev/dri"
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      syncthing:
        image: "lscr.io/linuxserver/syncthing:latest"
        volumes:
          - "syncthing-config:/config"
          - "/mnt/media:/data"
        ports:
          - "8384:8384"
          - "22000:22000/tcp"
    volumes:
      syncthing-config:
        driver: local
    "#);
}

/// #68: `raw` used to be flattened in on top of the built-in fields
/// without checking for collisions, so a `raw` key naming one of them
/// emitted that key *twice* in the same mapping — invalid YAML that
/// `docker compose config` rejects outright and Python's
/// `yaml.safe_load` silently reads last-wins. `raw` now wins outright
/// and the built-in is suppressed.
///
/// Note this test would fail on the old behavior at `yaml_value`'s own
/// parse step, not at the snapshot comparison: `serde_yaml_ng` rejects
/// a duplicate mapping key.
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
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: override
        container_name: boom
    ");
}

/// The warning is conditioned on there being something to lose, not on
/// which fields the service happens to declare: a service that generates
/// no labels at all has nothing for `raw { labels: ... }` to replace, so
/// it says nothing.
#[test]
fn raw_labels_on_a_service_with_no_computed_labels_say_nothing() {
    let program = parse(
        "service web {\n  \
           image \"nginx\"\n  \
           raw {\n    labels: [\"only.this=1\"]\n  }\n\
         }\n",
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

/// A service that writes no labels emits no `labels:` key at all.
/// `expose <port>` alone stays legal and says nothing about routing —
/// it's Compose's own `expose:` key.
#[test]
fn a_service_with_no_labels_emits_no_labels_key() {
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
/// bind-mount side, and the two go through different `ArrowMapHost` arms
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: "jellyfin/jellyfin:latest"
        volumes:
          - "media:/data:ro"
          - "/mnt/config:/config"
    volumes:
      media: ~
    "#);
}

/// A `dockerfile` switches `build:` from Compose's short form to its
/// long one, and `{{name}}` resolves in both halves.
#[test]
fn build_with_a_dockerfile_emits_the_long_form() {
    let yaml = generate_from(
        "service app {\n  \
           image \"app:latest\"\n  \
           build {\n    context: \"./{{name}}\"\n    dockerfile: \"Dockerfile.prod\"\n  }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      app:
        image: "app:latest"
        build:
          context: "./app"
          dockerfile: Dockerfile.prod
    "#);
}

/// The second half of #224: the requirement is checked against the
/// emitted document, so a hand-written `raw { image: ... }` satisfies
/// it. The issue reported this rejected even though the key it writes
/// is exactly the key being demanded.
#[test]
fn a_raw_supplied_image_satisfies_the_requirement() {
    let yaml = generate_from(
        "service foo {\n  raw {\n    image: \"test:latest\"\n    build: \"./foo\"\n  }\n}\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      foo:
        image: "test:latest"
        build: "./foo"
    "#);
}

/// A `raw` `build:` alone satisfies it too, and overrides a structured
/// one rather than emitting the key twice (#68's rule, applied to the
/// new field).
#[test]
fn a_raw_build_overrides_the_structured_one() {
    let yaml = generate_from(
        "service foo {\n  build \"./structured\"\n  raw {\n    build: \"./raw\"\n  }\n}\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      foo:
        build: "./raw"
    "#);
}

/// A `build` block with no `context` is refused: the context is the
/// whole of what there is to build.
#[test]
fn build_without_a_context_is_an_error() {
    let err = generate_err("service app {\n  build {\n    dockerfile: \"Dockerfile\"\n  }\n}\n");
    assert!(
        matches!(
            &err,
            CodegenError::BuildWithoutContext { service, .. } if service == "app"
        ),
        "got {err:?}"
    );
}

// ---- #243: a first-class, additive `labels` field ----

/// The other half of the preceding test: a service that *does* have
/// labels gets the warning, so the "nothing to lose" condition is a
/// real condition rather than the warning never firing.
#[test]
fn raw_labels_beside_real_labels_warn() {
    let program = parse(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \"com.example.owner\": \"platform-team\"\n  }\n  \
           raw {\n    labels: [\"only.this=1\"]\n  }\n\
         }\n",
    )
    .unwrap();
    let composed = compose(program).unwrap();
    let generated = generate(composed).unwrap();
    assert!(
        matches!(
            generated.warnings.as_slice(),
            [CodegenWarning::RawLabelsReplaceGenerated { service, .. }] if service == "web"
        ),
        "expected one raw-labels warning, got: {:?}",
        generated.warnings
    );
}

/// The warning keys on the `raw` entry named `labels` specifically, not
/// on there being any `raw` entry at all: a service with labels and a
/// `raw` block that names some *other* key loses nothing, so it says
/// nothing. Without this, a check that fired on every `raw` key would
/// look identical to the correct one in every other test here.
#[test]
fn a_raw_key_other_than_labels_does_not_warn() {
    let program = parse(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \"com.example.owner\": \"platform-team\"\n  }\n  \
           raw {\n    security_opt: [\"no-new-privileges:true\"]\n  }\n\
         }\n",
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

/// `raw`'s documented full-override semantics are unchanged (#243
/// deliberately leaves them alone): a `raw { labels: ... }` beside an
/// explicit `labels` still replaces the whole list, hand-written entries
/// included, because `raw` replaces the *emitted key* rather than any
/// one contributor to it.
#[test]
fn raw_labels_still_replace_explicit_labels_too() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \"com.example.owner\": \"platform-team\"\n  }\n  \
           raw {\n    labels: [\"only.this=1\"]\n  }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        labels:
          - only.this=1
    ");
}

/// `{{name}}` resolves in both halves of an entry, exactly as it does
/// for `env` — the interpolated text is what reaches the label, and so
/// what the collision and safety checks see.
#[test]
fn explicit_labels_interpolate_the_service_name() {
    let yaml = generate_from(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \"com.example.{{name}}.owner\": \"{{name}}-team\"\n  }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @"
    services:
      web:
        image: nginx
        labels:
          - com.example.web.owner=web-team
    ");
}

/// A newline in a key is writable since string escapes landed (#181),
/// and no label key can hold one for any legitimate reason.
#[test]
fn a_newline_in_a_label_key_is_rejected() {
    let err = generate_err(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \"com.example\\nowner\": \"y\"\n  }\n\
         }\n",
    );
    assert!(
        matches!(
            &err,
            CodegenError::UnsafeLabelKey { character, .. } if *character == '\n'
        ),
        "got {err:?}"
    );
}

// ---- #275: `.name` reads the real Docker name ----

/// A volume's name reads the same way, and the interpolated spelling
/// puts it mid-string — the two halves of #275 that aren't about
/// networks at all.
#[test]
fn a_volume_name_reads_and_interpolates_the_same_way() {
    let yaml = generate_from(
        "volume media {\n  name: \"media_store\"\n}\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin\"\n  \
           volume media -> \"/data\"\n  \
           labels {\n    \
             \"backup.volume\": media.name\n    \
             \"backup.path\": \"/mnt/{{media.name}}\"\n  }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: jellyfin/jellyfin
        volumes:
          - "media:/data"
        labels:
          - backup.volume=media_store
          - backup.path=/mnt/media_store
    volumes:
      media:
        name: media_store
    "#);
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
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      app:
        image: app
        networks:
          - default
      db:
        image: "postgres:15"
        networks:
          - default
    networks:
      default:
        name: shared_net
        external: true
    "#);
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
        "network proxy-net {\n  external\n  name: \"docker_default\"\n}\n\
         volume web-data {}\n\
         service web {\n  \
           image \"nginx\"\n  \
           volume web-data -> \"/data\"\n  \
           networks [proxy-net]\n  \
           raw {\n    \
             volumes: [\"web-data:/elsewhere\"]\n    \
             networks: [\"proxy-net\"]\n  \
           }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      web:
        image: nginx
        volumes:
          - "web-data:/elsewhere"
        networks:
          - proxy-net
    networks:
      proxy-net:
        name: docker_default
        external: true
    volumes:
      web-data: ~
    "#);
}

/// Two keys spelled differently in source can resolve to one key after
/// `{{name}}` interpolation, which the parser's own duplicate-key check
/// cannot see. Codegen catches it, since by then both are strings.
#[test]
fn two_explicit_labels_colliding_after_interpolation_is_an_error() {
    let err = generate_err(
        "service web {\n  \
           image \"nginx\"\n  \
           labels {\n    \
             \"a.{{name}}\": \"1\"\n    \
             \"a.web\": \"2\"\n  }\n\
         }\n",
    );
    assert!(
        matches!(&err, CodegenError::DuplicateLabelKey { key, .. } if key == "a.web"),
        "got {err:?}"
    );
}

/// The shape #275 exists for, end to end: one parameter serves the
/// `networks [$net]` entry, which wants the identifier, *and* the label
/// value, which wants the real Docker name. The two differ here on
/// purpose — an external network created by another Compose project
/// almost always carries a `name:` — because that difference is exactly
/// what the workaround this replaces got wrong, silently.
#[test]
fn a_field_access_emits_the_real_docker_network_name() {
    let yaml = generate_from(
        "network proxy {\n  external\n  name: \"docker_default\"\n}\n\
         template caddy(net, port) {\n  \
           networks [$net]\n  \
           expose $port\n  \
           labels {\n    \
             \"caddy.network\": $net.name\n    \
             \"caddy.upstream\": \"{{name}}:{{port}}\"\n  }\n\
         }\n\
         service jellyfin {\n  \
           image \"jellyfin/jellyfin\"\n  \
           with caddy { net: proxy, port: 8096 }\n\
         }\n",
    );
    assert_yaml_snapshot!(yaml_value(&yaml), @r#"
    services:
      jellyfin:
        image: jellyfin/jellyfin
        networks:
          - proxy
        expose:
          - 8096
        labels:
          - caddy.network=docker_default
          - "caddy.upstream=jellyfin:8096"
    networks:
      proxy:
        name: docker_default
        external: true
    "#);
}
