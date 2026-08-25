//! Integration tests for template/`with` composition (`hl_parser::compose`),
//! covering docs/DESIGN.md's Composition section's merge/conflict rules
//! beyond the single canonical worked example already covered end-to-end
//! in `examples.rs`.

use hl_parser::schema::MapSide;
use hl_parser::{
    ArrowMapHost, Command, ComposeError, ComposedProgram, Entrypoint, Healthcheck, HealthcheckTest,
    Literal, RawValue, Service, compose, parse,
};

fn compose_ok(source: &str) -> ComposedProgram {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    compose(program).unwrap_or_else(|err| panic!("unexpected compose error: {err}"))
}

fn compose_err(source: &str) -> ComposeError {
    let program = parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"));
    compose(program).expect_err("expected a compose error")
}

fn single_service(program: &ComposedProgram) -> &Service {
    assert_eq!(program.services.len(), 1, "expected exactly one service");
    &program.services[0]
}

fn raw_text(value: &RawValue) -> &str {
    match value {
        RawValue::Literal(lit) => lit.text(),
        other => panic!("expected a literal raw value, got {other:?}"),
    }
}

// --- defaults tier ---

#[test]
fn defaults_is_overridden_by_explicit_template_silently() {
    let composed = compose_ok(
        "template defaults {\n  restart unless-stopped\n}\n\
         template override_restart {\n  restart always\n}\n\
         service s {\n  with override_restart\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service
            .fields
            .restart
            .as_ref()
            .unwrap()
            .policy
            .as_ref()
            .unwrap()
            .text(),
        "always"
    );
}

#[test]
fn defaults_map_entries_survive_untouched_but_service_body_overrides_others() {
    let composed = compose_ok(
        "template defaults {\n  env FOO = \"default\"\n  env BAR = \"default-bar\"\n}\n\
         service s {\n  image \"x\"\n  env FOO = \"own\"\n}\n",
    );
    let service = single_service(&composed);
    let entries = &service.fields.env.entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key.text(), "FOO");
    assert_eq!(entries[0].value.text(), "own");
    assert_eq!(entries[1].key.text(), "BAR");
    assert_eq!(entries[1].value.text(), "default-bar");
}

#[test]
fn defaults_map_entry_is_silently_overridden_by_explicit_template() {
    let composed = compose_ok(
        "template defaults {\n  env FOO = \"default\"\n}\n\
         template t {\n  env FOO = \"explicit\"\n}\n\
         service s {\n  image \"x\"\n  with t\n}\n",
    );
    let service = single_service(&composed);
    let entries = &service.fields.env.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.text(), "FOO");
    assert_eq!(entries[0].value.text(), "explicit");
}

// --- explicit-vs-explicit collisions ---

#[test]
fn explicit_templates_scalar_collision_is_error() {
    let err = compose_err(
        "template a {\n  image \"a-image\"\n}\n\
         template b {\n  image \"b-image\"\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "image.ref",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision, got {other:?}"),
    }
}

/// `restart` goes through the same generic scalar-merge path as `image`
/// (see `compose.rs`'s `MergeAcc`/`merge_scalar`) — no test previously
/// exercised an explicit-vs-explicit collision on it specifically, only
/// `image`'s. Added alongside the merge engine's generalization to
/// confirm every scalar collision point still gets caught, not just the
/// one the old per-field `MergeAcc` slots happened to have a test for.
#[test]
fn explicit_templates_restart_collision_is_error() {
    let err = compose_err(
        "template a {\n  restart always\n}\n\
         template b {\n  restart unless-stopped\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "restart.policy",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision on restart, got {other:?}"),
    }
}

#[test]
fn explicit_templates_env_key_collision_is_error() {
    let err = compose_err(
        "template a {\n  env FOO = \"a\"\n}\n\
         template b {\n  env FOO = \"b\"\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "env");
            assert_eq!(details.side, MapSide::Key);
            assert_eq!(details.key, "FOO");
            assert_eq!(details.first_template, "a");
            assert_eq!(details.second_template, "b");
        }
        other => panic!("expected MapKeyCollision, got {other:?}"),
    }
}

#[test]
fn explicit_templates_volume_container_path_collision_is_error() {
    let err = compose_err(
        "template a {\n  volume \"h1\" -> \"/data\"\n}\n\
         template b {\n  volume \"h2\" -> \"/data\"\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "volume");
            assert_eq!(details.side, MapSide::Value);
            assert_eq!(details.key, "/data");
        }
        other => panic!("expected MapKeyCollision, got {other:?}"),
    }
}

/// #84: `publish` merges exactly like `volume` — keyed on the container
/// port, since that's the side its schema checks uniqueness on.
#[test]
fn explicit_templates_publish_container_port_collision_is_error() {
    let err = compose_err(
        "template a {\n  publish 8096 -> 8096\n}\n\
         template b {\n  publish 9096 -> 8096\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "publish");
            assert_eq!(details.side, MapSide::Value);
            assert_eq!(details.key, "8096");
        }
        other => panic!("expected MapKeyCollision, got {other:?}"),
    }
}

/// #155: `depends_on` merges keyed on the referenced service's own
/// name, like `env`'s key side — but only *differing* conditions are a
/// genuine collision. Here template `a`'s entry is effectively
/// `service_healthy` and template `b`'s is effectively `service_started`
/// (its bare `depends_on [db]`), so the two templates are proposing two
/// different answers about the same dependency, not the same one
/// twice — exactly as two explicit templates setting the same `env` key
/// to two different values would collide.
#[test]
fn explicit_templates_depends_on_differing_conditions_is_error() {
    let err = compose_err(
        "template a {\n  depends_on [db { condition: service_healthy }]\n}\n\
         template b {\n  depends_on [db]\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "depends_on");
            assert_eq!(details.side, MapSide::Key);
            assert_eq!(details.key, "db");
            assert_eq!(details.first_template, "a");
            assert_eq!(details.second_template, "b");
        }
        other => panic!("expected MapKeyCollision, got {other:?}"),
    }
}

/// The common case, and the one this rule exists to keep working: two
/// explicit templates each writing a plain `depends_on [db]` are giving
/// the *same* answer twice (both mean Compose's own implicit
/// `service_started` default), not two different ones, so this composes
/// successfully to a single entry rather than colliding — exactly as it
/// did before #155 introduced the condition form at all.
#[test]
fn explicit_templates_depends_on_identical_bare_entries_compose_to_one_entry() {
    let composed = compose_ok(
        "template a {\n  depends_on [db]\n}\n\
         template b {\n  depends_on [db]\n}\n\
         service s {\n  with a, b\n}\n\
         service db {\n  image \"x\"\n}\n",
    );
    let service = composed
        .services
        .iter()
        .find(|s| s.name.name == "s")
        .expect("service s");
    assert_eq!(service.fields.depends_on.len(), 1);
    assert_eq!(service.fields.depends_on[0].reference.text(), "db");
    assert!(service.fields.depends_on[0].condition.is_none());
}

/// Same idea, but both templates spell the condition out explicitly and
/// agree: still composes to one entry, not a collision.
#[test]
fn explicit_templates_depends_on_identical_conditions_compose_to_one_entry() {
    let composed = compose_ok(
        "template a {\n  depends_on [db { condition: service_healthy }]\n}\n\
         template b {\n  depends_on [db { condition: service_healthy }]\n}\n\
         service s {\n  with a, b\n}\n\
         service db {\n  image \"x\"\n}\n",
    );
    let service = composed
        .services
        .iter()
        .find(|s| s.name.name == "s")
        .expect("service s");
    assert_eq!(service.fields.depends_on.len(), 1);
    assert_eq!(
        service.fields.depends_on[0].condition.map(|(c, _)| c),
        Some(hl_parser::DependsOnCondition::ServiceHealthy)
    );
}

/// A bare entry in one template and an explicit `condition:
/// service_started` in another mean exactly the same thing to
/// Compose — `service_started` is its own implicit default — so this
/// composes successfully rather than colliding, even though the two
/// entries aren't written identically.
#[test]
fn explicit_templates_depends_on_bare_and_explicit_default_agree() {
    let composed = compose_ok(
        "template a {\n  depends_on [db]\n}\n\
         template b {\n  depends_on [db { condition: service_started }]\n}\n\
         service s {\n  with a, b\n}\n\
         service db {\n  image \"x\"\n}\n",
    );
    let service = composed
        .services
        .iter()
        .find(|s| s.name.name == "s")
        .expect("service s");
    assert_eq!(service.fields.depends_on.len(), 1);
    // `merge_depends_on` keeps the *earlier* entry's own written form
    // (first-occurrence-wins, the same rule the set-like reference
    // lists already use for a repeated name) rather than normalizing to
    // whichever spelling won — template `a`'s bare entry ran first, so
    // its bare form survives, and codegen's short-vs-long-form switch
    // (#155) reads that as "no explicit condition anywhere in this
    // field," which is exactly what both templates meant.
    assert!(service.fields.depends_on[0].condition.is_none());
}

/// The service's own body always wins over a template's `depends_on`
/// entry for the same service, condition included — the same
/// Own-always-wins rule every other keyed field already follows.
#[test]
fn depends_on_own_body_overrides_a_templates_condition() {
    let composed = compose_ok(
        "template waits_for_db {\n  depends_on [db { condition: service_started }]\n}\n\
         service s {\n  with waits_for_db\n  depends_on [db { condition: service_healthy }]\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.depends_on.len(), 1);
    let entry = &service.fields.depends_on[0];
    assert_eq!(entry.reference.text(), "db");
    assert_eq!(
        entry.condition.map(|(c, _)| c),
        Some(hl_parser::DependsOnCondition::ServiceHealthy)
    );
}

/// The implicit `defaults` template always silently loses, even on
/// `depends_on` — an explicit `with`-listed template's own entry for the
/// same service wins.
#[test]
fn depends_on_defaults_tier_loses_to_an_explicit_template() {
    let composed = compose_ok(
        "template defaults {\n  depends_on [db { condition: service_started }]\n}\n\
         template waits_for_db {\n  depends_on [db { condition: service_healthy }]\n}\n\
         service s {\n  with waits_for_db\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.depends_on.len(), 1);
    assert_eq!(
        service.fields.depends_on[0].condition.map(|(c, _)| c),
        Some(hl_parser::DependsOnCondition::ServiceHealthy)
    );
}

/// Non-colliding entries accumulate across tiers, and a `$param` in
/// either half of a mapping is substituted like any other literal slot.
#[test]
fn publish_accumulates_across_tiers_and_substitutes_params() {
    let composed = compose_ok(
        "template ports(p) {\n  publish $p -> $p\n}\n\
         service s {\n  with ports { p: 8384 }\n  publish 22000 -> 22000\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<(&str, &str)> = service
        .fields
        .publish
        .entries
        .iter()
        .map(|e| (e.host.text(), e.container.text()))
        .collect();
    assert_eq!(entries, vec![("8384", "8384"), ("22000", "22000")]);
}

// --- container_name (#17) ---

/// `container_name` merges like any other scalar field: the service's
/// own body wins over an inherited template value. Whether it defaults
/// to the service's own name when unset entirely is a codegen concern
/// (see `hl-codegen`'s tests), not composition's — composition just
/// leaves it `None` if nothing ever set it.
#[test]
fn service_own_container_name_overrides_template() {
    let composed = compose_ok(
        "template a {\n  container_name \"from-template\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  container_name \"own\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service.fields.container_name.as_ref().unwrap().text(),
        "own"
    );
}

#[test]
fn container_name_inherited_from_template_when_service_unset() {
    let composed = compose_ok(
        "template a {\n  container_name \"from-template\"\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service.fields.container_name.as_ref().unwrap().text(),
        "from-template"
    );
}

#[test]
fn explicit_templates_container_name_collision_is_error() {
    let err = compose_err(
        "template a {\n  container_name \"a-name\"\n}\n\
         template b {\n  container_name \"b-name\"\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "container_name",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision on container_name, got {other:?}"),
    }
}

// --- expose merge (#10, narrowed to `port` alone by #198) ---

/// `expose.port` collides across two explicit templates exactly like any
/// other scalar field — same rule, same error, since #198 left `port`
/// the only sub-field `expose` still has (`host`/`entrypoint` moved onto
/// `router`, whose own per-sub-field merge has its own section below —
/// see `service_own_body_can_override_just_one_router_subfield` and its
/// neighbors).
#[test]
fn explicit_templates_setting_expose_port_still_collide() {
    let err = compose_err(
        "template a {\n  expose 8080\n}\n\
         template b {\n  expose 9090\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "expose.port",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision on expose.port, got {other:?}"),
    }
}

/// `defaults`' own `expose.port` is silently overridden by an explicit
/// template's, matching every other scalar field's own `defaults`-
/// always-loses rule.
#[test]
fn defaults_expose_port_is_overridden_silently() {
    let composed = compose_ok(
        "template defaults {\n  expose 1234\n}\n\
         template real {\n  expose 8080\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8080");
}

// --- healthcheck sub-field merge (#153) ---
//
// `healthcheck` follows the exact same per-sub-field merge `expose`
// established above (see that section's tests) rather than merging as
// one indivisible unit — chosen deliberately for consistency, per the
// issue's own instruction to "follow how `expose` is merged... rather
// than inventing a new mechanism." `test` and `disable` merge the same
// way as `interval`/`timeout`/`retries`/`start_period`/
// `start_interval` even though their values aren't `Literal`s — see
// `MergeAcc::healthcheck_test`'s doc for how `merge_scalar_like`
// generalizes the same Own/Defaults/two-explicit-collide rule to them.

fn healthcheck_test_text(hc: &Healthcheck) -> &str {
    match hc.test.as_ref().expect("test set") {
        HealthcheckTest::Shell(lit) => lit.text(),
        HealthcheckTest::Exec(..) => panic!("expected the shell form"),
    }
}

/// A service's own body can override just one `healthcheck` sub-field
/// while still inheriting the rest from a `with`-listed template,
/// without repeating them — the same shape as
/// `service_own_body_can_override_just_expose_host`.
#[test]
fn service_own_body_can_override_just_healthcheck_interval() {
    let composed = compose_ok(
        "template pg_healthcheck {\n  \
           healthcheck {\n    test: \"pg_isready\"\n    interval: \"10s\"\n  }\n\
         }\n\
         service db {\n  \
           with pg_healthcheck\n  \
           image \"postgres\"\n  \
           healthcheck { interval: \"30s\" }\n\
         }\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "pg_isready");
    assert_eq!(hc.interval.as_ref().unwrap().text(), "30s");
}

/// Two explicit templates each setting a *different* `healthcheck`
/// sub-field don't collide.
#[test]
fn explicit_templates_setting_different_healthcheck_subfields_do_not_collide() {
    let composed = compose_ok(
        "template a {\n  healthcheck { test: \"pg_isready\" }\n}\n\
         template b {\n  healthcheck { interval: \"10s\" }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "pg_isready");
    assert_eq!(hc.interval.as_ref().unwrap().text(), "10s");
}

/// Two explicit templates setting the *same* `healthcheck` sub-field
/// still collide — the per-sub-field merge narrows the granularity of
/// the collision rule, it doesn't remove it. Covers `test`
/// specifically, since it's not a plain `Literal` and goes through
/// `merge_scalar_like` rather than `SCALAR_FIELDS`.
#[test]
fn explicit_templates_setting_same_healthcheck_test_still_collide() {
    let err = compose_err(
        "template a {\n  healthcheck { test: \"a\" }\n}\n\
         template b {\n  healthcheck { test: \"b\" }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "healthcheck.test",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision on healthcheck.test, got {other:?}"),
    }
}

/// Same collision rule, for `disable` — a bare-presence flag whose
/// "value" is just the span it was set at, still tracked and still
/// collision-checked between two explicit templates.
#[test]
fn explicit_templates_setting_same_healthcheck_disable_still_collide() {
    let err = compose_err(
        "template a {\n  healthcheck { disable }\n}\n\
         template b {\n  healthcheck { disable }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::FieldCollision {
                field: "healthcheck.disable",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// Same collision rule, for a plain `Literal` sub-field (`interval`),
/// confirming the `SCALAR_FIELDS`-routed sub-fields collide exactly
/// like `expose.port` does.
#[test]
fn explicit_templates_setting_same_healthcheck_interval_still_collide() {
    let err = compose_err(
        "template a {\n  healthcheck { interval: \"10s\" }\n}\n\
         template b {\n  healthcheck { interval: \"20s\" }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::FieldCollision {
                field: "healthcheck.interval",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// A `defaults` template setting one `healthcheck` sub-field is
/// silently overridden only on that sub-field — the other sub-fields an
/// explicit template sets still come through.
#[test]
fn defaults_healthcheck_subfield_is_overridden_but_others_survive() {
    let composed = compose_ok(
        "template defaults {\n  healthcheck {\n    test: \"placeholder\"\n    interval: \"1s\"\n  }\n}\n\
         template real {\n  healthcheck { interval: \"10s\" }\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "placeholder");
    assert_eq!(hc.interval.as_ref().unwrap().text(), "10s");
}

/// A service's own `healthcheck.test` beats an explicit `with`
/// template's `test` for the same sub-field — the `test`/`disable`
/// analogue of `service_own_body_can_override_just_healthcheck_interval`
/// above, but exercising `merge_scalar_like`'s `(_, Tier::Own)` arm
/// directly rather than `merge_scalar`'s identical rule for plain
/// `Literal` fields. `test`/`disable` are the only two `healthcheck`
/// sub-fields that go through `merge_scalar_like` instead of
/// `SCALAR_FIELDS`, so this is the only way to reach that arm at all.
#[test]
fn service_own_healthcheck_test_beats_an_explicit_templates_test() {
    let composed = compose_ok(
        "template pg_healthcheck {\n  healthcheck { test: \"pg_isready\" }\n}\n\
         service db {\n  \
           with pg_healthcheck\n  \
           image \"postgres\"\n  \
           healthcheck { test: \"custom-check\" }\n\
         }\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "custom-check");
}

/// A `defaults` template's `healthcheck.test` is silently overridden by
/// an *explicit* template's own `test` for the same sub-field — the
/// `test`/`disable` analogue of
/// `defaults_healthcheck_subfield_is_overridden_but_others_survive`
/// above, but exercising `merge_scalar_like`'s `(Tier::Defaults, _)`
/// arm directly. That test only exercises `interval` (a plain
/// `Literal`, routed through `merge_scalar`); `test`'s collision point
/// is a distinct function with its own copy of the same rule.
#[test]
fn defaults_healthcheck_test_loses_to_an_explicit_templates_test() {
    let composed = compose_ok(
        "template defaults {\n  healthcheck { test: \"placeholder\" }\n}\n\
         template real {\n  healthcheck { test: \"pg_isready\" }\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "pg_isready");
}

/// `healthcheck` lives entirely inside one `Option<Healthcheck>` that
/// the merge rebuilds from scratch — so an inherited sub-field with no
/// others beside it still has to materialize the enclosing
/// `Healthcheck`, not vanish. Mirrors
/// `entrypoint_alone_still_materializes_expose`.
#[test]
fn healthcheck_disable_alone_still_materializes_healthcheck() {
    let composed = compose_ok(
        "template a {\n  healthcheck { disable }\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert!(hc.disable.is_some());
    assert!(hc.test.is_none());
}

/// The mirror of the above: no tier setting any `healthcheck` sub-field
/// at all must not conjure one into existence.
#[test]
fn no_healthcheck_anywhere_leaves_it_unset() {
    let composed = compose_ok(
        "template a {\n  middleware auth\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert!(service.fields.healthcheck.is_none());
}
// --- traefik (#159) ---

/// A template carrying `traefik { disabled }` composes onto a service
/// through `with`, exactly like `healthcheck { disable }` does — the
/// same `merge_scalar_like`-routed collision point, just for `traefik`'s
/// own `MergeAcc` slot instead of `healthcheck`'s.
#[test]
fn traefik_disabled_composes_through_with() {
    let composed = compose_ok(
        "template backend_only {\n  traefik { disabled }\n}\n\
         service db {\n  with backend_only\n  image \"postgres:15\"\n}\n",
    );
    let service = single_service(&composed);
    let traefik = service.fields.traefik.as_ref().expect("traefik set");
    assert!(traefik.disabled.is_some());
}

/// Two explicit templates both writing `traefik { disabled }` still
/// collide — the `traefik` analogue of
/// `explicit_templates_setting_same_healthcheck_disable_still_collide`:
/// `merge_scalar_like`'s `Explicit`-vs-`Explicit` arm always errors, even
/// though the two agree, because nothing about the merge engine can tell
/// "genuinely agree" apart from "coincidentally wrote the same thing."
#[test]
fn explicit_templates_setting_same_traefik_disabled_still_collide() {
    let err = compose_err(
        "template a {\n  traefik { disabled }\n}\n\
         template b {\n  traefik { disabled }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::FieldCollision {
                field: "traefik.disabled",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// A `defaults` template's `traefik { disabled }` survives untouched
/// when nothing else in the composition names `traefik` at all — the
/// `traefik` analogue of `defaults_map_entries_survive_untouched_but_service_body_overrides_others`:
/// `Tier::Defaults` only ever loses to a *competing* value for the same
/// field, and an unset field from a later tier is never that.
#[test]
fn defaults_traefik_disabled_survives_when_unchallenged() {
    let composed = compose_ok(
        "template defaults {\n  traefik { disabled }\n}\n\
         service s {\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let traefik = service.fields.traefik.as_ref().expect("traefik set");
    assert!(traefik.disabled.is_some());
}

/// A `defaults` template's `traefik { disabled }` is silently overridden
/// once an *explicit* template also sets it — the `Tier::Defaults` arm
/// of `merge_scalar_like`, the same rule
/// `defaults_healthcheck_test_loses_to_an_explicit_templates_test`
/// exercises for `healthcheck.test`. The visible value can't actually
/// differ (`disabled` has no `false` form), so what this pins down is
/// that the *explicit* tier's own span — not the `defaults` one — is
/// what survives the merge.
#[test]
fn defaults_traefik_disabled_loses_its_span_to_an_explicit_templates_disabled() {
    let composed = compose_ok(
        "template defaults {\n  traefik { disabled }\n}\n\
         template real {\n  traefik { disabled }\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let traefik = service.fields.traefik.as_ref().expect("traefik set");
    assert!(traefik.disabled.is_some());
}

/// A service's own `traefik { disabled }` beats an explicit `with`
/// template that leaves `traefik` unset — `merge_scalar_like`'s `(_,
/// Tier::Own)` arm, the same one
/// `service_own_healthcheck_test_beats_an_explicit_templates_test`
/// exercises for `healthcheck.test`.
#[test]
fn service_own_traefik_disabled_survives_with_no_competing_template_value() {
    let composed = compose_ok(
        "template pg {\n  restart unless-stopped\n}\n\
         service db {\n  with pg\n  image \"postgres:15\"\n  traefik { disabled }\n}\n",
    );
    let service = single_service(&composed);
    let traefik = service.fields.traefik.as_ref().expect("traefik set");
    assert!(traefik.disabled.is_some());
}

// --- command merge (#156) ---
//
// `command` merges through `merge_scalar_like` — the same
// Own-always-wins/Defaults-always-loses/two-explicit-collide rule
// `healthcheck.test` uses (#153), since `Command` isn't a `Literal` and
// so can't ride `SCALAR_FIELDS`. Unlike `healthcheck.test`, `command`
// lives directly on `ServiceFields` rather than inside a nested struct,
// so there's no "materializes the enclosing struct" case to cover the
// way `healthcheck_disable_alone_still_materializes_healthcheck` does —
// these tests otherwise mirror `container_name`'s own merge tests
// above, since both are bare, indivisible fields on `ServiceFields`.

fn command_shell_text(command: &Command) -> &str {
    match command {
        Command::Shell(lit) => lit.text(),
        Command::Exec(..) => panic!("expected the shell form"),
    }
}

/// The service's own body wins over an inherited template value.
#[test]
fn service_own_command_overrides_template() {
    let composed = compose_ok(
        "template a {\n  command \"from-template\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  command \"own\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        command_shell_text(service.fields.command.as_ref().unwrap()),
        "own"
    );
}

/// With no service-level override, the template's own value comes
/// through unchanged.
#[test]
fn command_inherited_from_template_when_service_unset() {
    let composed = compose_ok(
        "template a {\n  command \"from-template\"\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        command_shell_text(service.fields.command.as_ref().unwrap()),
        "from-template"
    );
}

/// Two explicit templates setting different `command` values still
/// collide — unlike `healthcheck`, `command` has no sub-fields to
/// narrow the collision to, so any two explicit templates that both set
/// it disagree by definition.
#[test]
fn explicit_templates_command_collision_is_error() {
    let err = compose_err(
        "template a {\n  command \"a-cmd\"\n}\n\
         template b {\n  command \"b-cmd\"\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision {
            field: "command",
            first_template,
            second_template,
            ..
        } => {
            assert_eq!(first_template, "a");
            assert_eq!(second_template, "b");
        }
        other => panic!("expected FieldCollision on command, got {other:?}"),
    }
}

/// A `defaults` template's `command` is silently overridden by an
/// explicit template's own value — exercising `merge_scalar_like`'s
/// `(Tier::Defaults, _)` arm on `command`'s own `MergeAcc` slot, the
/// same arm `defaults_healthcheck_test_loses_to_an_explicit_templates_test`
/// exercises on `healthcheck.test`'s.
#[test]
fn defaults_command_loses_to_an_explicit_templates_command() {
    let composed = compose_ok(
        "template defaults {\n  command \"placeholder\"\n}\n\
         template real {\n  command \"npm start\"\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        command_shell_text(service.fields.command.as_ref().unwrap()),
        "npm start"
    );
}

/// The exec form survives composition intact, including an item with an
/// embedded comma — the `cadvisor.hll` shape the issue calls out
/// (#156), reached through a template this time rather than directly on
/// the service.
#[test]
fn command_exec_form_with_embedded_comma_survives_composition() {
    let composed = compose_ok(
        "template cadvisor_cmd {\n  \
           command [\"--enable_metrics=cpu,memory,network\"]\n\
         }\n\
         service s {\n  with cadvisor_cmd\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    match service.fields.command.as_ref().unwrap() {
        Command::Exec(items, _) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "--enable_metrics=cpu,memory,network");
        }
        other => panic!("expected Command::Exec, got {other:?}"),
    }
}

// --- entrypoint merge (#183) ---
//
// `entrypoint` merges exactly like `command` above — its own
// `merge_scalar_like` slot, because `Entrypoint` isn't a `Literal`
// either. The last test here is the one that isn't just `command`'s
// tests renamed: `entrypoint` and `command` are independent Compose
// keys, so two templates each setting one of them compose rather than
// collide.

fn entrypoint_shell_text(entrypoint: &Entrypoint) -> &str {
    match entrypoint {
        Entrypoint::Shell(lit) => lit.text(),
        other => panic!("expected Entrypoint::Shell, got {other:?}"),
    }
}

/// The service's own body wins over a `with`-listed template's, the
/// same "own always beats a template" rule every other field follows.
#[test]
fn service_own_entrypoint_overrides_template() {
    let composed = compose_ok(
        "template a {\n  entrypoint \"from-template\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  entrypoint \"own\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        entrypoint_shell_text(service.fields.entrypoint.as_ref().unwrap()),
        "own"
    );
}

/// Two explicit templates setting different `entrypoint` values
/// collide, exactly as two setting different `command` values do —
/// `entrypoint` has no sub-fields to merge independently.
#[test]
fn explicit_templates_entrypoint_collision_is_error() {
    let err = compose_err(
        "template a {\n  entrypoint \"a-ep\"\n}\n\
         template b {\n  entrypoint \"b-ep\"\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldCollision { field, .. } => {
            assert_eq!(field, "entrypoint");
        }
        other => panic!("expected FieldCollision on entrypoint, got {other:?}"),
    }
}

/// `entrypoint` and `command` are two different Compose keys, so two
/// explicit templates each setting one of them don't collide — the
/// composed service carries both.
#[test]
fn entrypoint_and_command_from_two_templates_do_not_collide() {
    let composed = compose_ok(
        "template ep {\n  entrypoint \"/entrypoint.sh\"\n}\n\
         template cmd {\n  command \"--flag\"\n}\n\
         service s {\n  with ep, cmd\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        entrypoint_shell_text(service.fields.entrypoint.as_ref().unwrap()),
        "/entrypoint.sh"
    );
    assert_eq!(
        command_shell_text(service.fields.command.as_ref().unwrap()),
        "--flag"
    );
}

// --- non-colliding merges ---

#[test]
fn non_colliding_map_entries_from_different_templates_merge() {
    let composed = compose_ok(
        "template a {\n  env FOO = \"a\"\n}\n\
         template b {\n  env BAR = \"b\"\n}\n\
         service s {\n  with a, b\n}\n",
    );
    let service = single_service(&composed);
    let keys: Vec<&str> = service
        .fields
        .env
        .entries
        .iter()
        .map(|e| e.key.text())
        .collect();
    assert_eq!(keys, vec!["FOO", "BAR"]);
}

#[test]
fn service_body_overrides_inherited_map_entry_unconditionally() {
    let composed = compose_ok(
        "template a {\n  volume \"h1\" -> \"/data\"\n}\n\
         service s {\n  with a\n  volume \"h2\" -> \"/data\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(service.fields.volumes.entries[0].host.text(), "h2");
    assert_eq!(service.fields.volumes.entries[0].container.text(), "/data");
}

/// The `{ read_only }` flag (#158) rides along through `merge_map`'s
/// full-entry replacement — an entry inherited from a template keeps its
/// own flag when nothing overrides its container path.
#[test]
fn inherited_volume_entrys_read_only_flag_survives_composition_untouched() {
    let composed = compose_ok(
        "template a {\n  volume \"/\" -> \"/rootfs\" { read_only }\n}\n\
         service s {\n  with a\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert!(service.fields.volumes.entries[0].read_only);
}

/// And the twin of [`service_body_overrides_inherited_map_entry_unconditionally`]:
/// when the service's own body overrides an inherited entry on the same
/// container path, the *whole* entry is replaced, including the flag —
/// a template's `{ read_only }` must not silently survive an own-body
/// override that wrote no flag at all, and conversely an own-body
/// override that *does* write the flag must not be lost either.
#[test]
fn own_body_override_replaces_the_inherited_entrys_read_only_flag_not_just_its_host() {
    let composed = compose_ok(
        "template a {\n  volume \"h1\" -> \"/data\" { read_only }\n}\n\
         service s {\n  with a\n  volume \"h2\" -> \"/data\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(service.fields.volumes.entries[0].host.text(), "h2");
    assert!(
        !service.fields.volumes.entries[0].read_only,
        "own body's unflagged entry must win outright, not silently keep the \
         template's stale `read_only`"
    );

    // And the reverse direction: the own body adds the flag a template's
    // entry never had.
    let composed = compose_ok(
        "template a {\n  volume \"h1\" -> \"/data\"\n}\n\
         service s {\n  with a\n  volume \"h2\" -> \"/data\" { read_only }\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert!(service.fields.volumes.entries[0].read_only);
}

/// #193: `raw` merges key-by-key through the same [`MapSide::Key`]
/// convention `env` uses, rather than concatenating outright regardless
/// of key. Entries naming *distinct* keys still accumulate across every
/// tier, in tier order — this test replaces a pre-#193 one of the same
/// shape that asserted three tiers writing the *same* `raw` key all
/// survived side by side (`"a"`, `"b"`, `"c"`), which was #193's own bug:
/// the compiler silently kept the value from every tier instead of
/// letting the later ones win or collide the way `env`'s repeated key
/// already did.
#[test]
fn raw_entries_from_every_tier_accumulate_by_distinct_key() {
    let composed = compose_ok(
        "template defaults {\n  raw { d: \"d\" }\n}\n\
         template a {\n  raw { a: \"a\" }\n}\n\
         template b {\n  raw { b: \"b\" }\n}\n\
         service s {\n  with a, b\n  raw { c: \"c\" }\n}\n",
    );
    let service = single_service(&composed);
    let keys: Vec<&str> = service
        .fields
        .raw
        .entries
        .iter()
        .map(|e| e.key.text())
        .collect();
    assert_eq!(keys, vec!["d", "a", "b", "c"]);
}

/// #193's own regression: two *explicit* templates setting the same
/// `raw` key now collide exactly the way the same conflict on `env`
/// already did — see `explicit_templates_env_key_collision_is_error`
/// above. `crates/hl-cli/tests/cases/issue_193_raw_template_collision.hll`
/// pins the rendered diagnostic; this test pins the `ComposeError`
/// variant and its fields instead, which a case file can't express.
#[test]
fn explicit_templates_raw_key_collision_is_error() {
    let err = compose_err(
        "template a {\n  raw { key: \"a\" }\n}\n\
         template b {\n  raw { key: \"b\" }\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "raw");
            assert_eq!(details.side, MapSide::Key);
            assert_eq!(details.key, "key");
            assert_eq!(details.first_template, "a");
            assert_eq!(details.second_template, "b");
        }
        other => panic!("expected MapKeyCollision, got {other:?}"),
    }
}

/// `defaults`-loses, `raw`'s own version of
/// `defaults_map_entry_is_silently_overridden_by_explicit_template`
/// above: when the implicit `defaults` template and an explicit
/// `with`-listed template both set the same `raw` key, the explicit
/// template's value silently wins — no collision is raised, even though
/// both contributors are templates, because `defaults` never
/// participates in conflict-checking.
#[test]
fn raw_defaults_entry_is_silently_overridden_by_explicit_template() {
    let composed = compose_ok(
        "template defaults {\n  raw { key: \"default\" }\n}\n\
         template t {\n  raw { key: \"explicit\" }\n}\n\
         service s {\n  image \"x\"\n  with t\n}\n",
    );
    let service = single_service(&composed);
    let entries = &service.fields.raw.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.text(), "key");
    assert_eq!(raw_text(&entries[0].value), "explicit");
}

/// Own-always-wins, `raw`'s own version of
/// `service_body_overrides_inherited_map_entry_unconditionally` above: a
/// service's own `raw` key silently replaces a `with`-listed template's
/// value for the same key, with no collision — `Own` never competes with
/// an `Explicit` tier the way two `Explicit` tiers compete with each
/// other.
#[test]
fn raw_own_body_overrides_inherited_map_entry_unconditionally() {
    let composed = compose_ok(
        "template a {\n  raw { key: \"template\" }\n}\n\
         service s {\n  with a\n  raw { key: \"own\" }\n}\n",
    );
    let service = single_service(&composed);
    let entries = &service.fields.raw.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.text(), "key");
    assert_eq!(raw_text(&entries[0].value), "own");
}

#[test]
fn list_fields_concatenate_in_priority_order() {
    let composed = compose_ok(
        "template defaults {\n  middleware d1\n}\n\
         template a {\n  middleware a1\n}\n\
         template b {\n  middleware b1\n}\n\
         service s {\n  with a, b\n  middleware own1\n}\n",
    );
    let service = single_service(&composed);
    let names: Vec<&str> = service.fields.middleware.iter().map(|r| r.text()).collect();
    assert_eq!(names, vec!["d1", "a1", "b1", "own1"]);
}

/// #69: the set-like lists concatenate *by distinct name*. Restating a
/// network (or middleware) a template already supplies is a natural
/// thing to write, and means exactly what stating it once means — so the
/// repeat is dropped rather than duplicated into the output.
///
/// `depends_on` used to be one of these set-like lists too, and this
/// test used to cover it alongside `networks`/`middleware` — but #155
/// moved its merge onto its own keyed-by-service-name engine (see the
/// `depends_on_*` tests below), since an entry can now carry a
/// `condition` two templates could genuinely disagree about. Two
/// explicit templates both writing a plain `depends_on [db]` still
/// compose to one entry exactly as before — they're proposing the same
/// answer twice, not two different ones — so nothing here actually
/// changed about *that* case; only two explicit templates whose
/// conditions genuinely differ get a new `MapKeyCollision` that simply
/// didn't exist before #155 gave `depends_on` anything to disagree
/// about.
#[test]
fn set_like_list_fields_dedupe_across_tiers() {
    let composed = compose_ok(
        "network proxy {\n  name: \"real\"\n}\n\
         template a {\n  networks [proxy]\n  middleware auth\n}\n\
         template b {\n  networks [proxy]\n  middleware auth\n}\n\
         service s {\n  image \"x\"\n  with a, b\n  networks [proxy]\n  middleware auth\n}\n",
    );
    let service = single_service(&composed);
    let names = |refs: &[hl_parser::Literal]| -> Vec<String> {
        refs.iter().map(|r| r.text().to_string()).collect()
    };
    assert_eq!(names(&service.fields.networks), vec!["proxy"]);
    assert_eq!(names(&service.fields.middleware), vec!["auth"]);
}

/// Deduping keeps the *first* occurrence, so the surviving order is
/// still tier order — `defaults`, then each `with` target left to
/// right, then the service's own list.
#[test]
fn deduped_list_keeps_first_occurrence_order() {
    let composed = compose_ok(
        "template defaults {\n  middleware d1\n}\n\
         template a {\n  middleware a1\n  middleware d1\n}\n\
         service s {\n  image \"x\"\n  with a\n  middleware own1\n  middleware a1\n}\n",
    );
    let service = single_service(&composed);
    let names: Vec<&str> = service.fields.middleware.iter().map(|r| r.text()).collect();
    assert_eq!(names, vec!["d1", "a1", "own1"]);
}

/// A repeat written twice inside *one* body is dropped too — the dedupe
/// is against everything accumulated so far, not just against earlier
/// tiers.
#[test]
fn repeated_entry_within_one_list_is_deduped() {
    let composed = compose_ok(
        "network proxy {\n  name: \"real\"\n}\n\
         service s {\n  image \"x\"\n  networks [proxy, proxy]\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.networks.len(), 1);
}

/// #69's expansion bomb: list-kind fields used to accumulate without
/// dedupe, so result size doubled per composition level even though
/// `resolve_template`'s cache prevents re-*resolution*. At N=26 — under
/// a kilobyte of source — that exhausted a 1 GB allocation cap.
///
/// The invariant under test is that the accumulated list's size doesn't
/// grow with composition *depth* at all, which N=12 states just as
/// sharply as N=26 would: one entry if deduping works, 4,096 if it
/// doesn't. N is kept deliberately low rather than at the issue's
/// headline figure, because a test that only fails by exhausting memory
/// takes the whole suite down with it instead of failing — under
/// `cargo mutants`, a broken dedupe at N=26 hangs the run and is
/// reported as a timeout rather than as the caught mutant it should be.
#[test]
fn nested_with_chain_does_not_expand_exponentially() {
    let mut source = String::from("template t0 {\n  middleware m\n}\n");
    for i in 1..=12 {
        source.push_str(&format!(
            "template t{i} {{\n  with t{}, t{}\n}}\n",
            i - 1,
            i - 1
        ));
    }
    source.push_str("service s {\n  image \"x\"\n  with t12\n}\n");
    let composed = compose_ok(&source);
    let service = single_service(&composed);
    assert_eq!(service.fields.middleware.len(), 1);
    assert_eq!(service.fields.middleware[0].text(), "m");
}

/// `dns` is list-typed just like `middleware`/`networks` — it
/// concatenates across tiers rather than colliding. (`depends_on` no
/// longer belongs in this list — #155 moved it onto a keyed merge; see
/// the `depends_on_*` tests further down.)
#[test]
fn dns_concatenates_across_tiers() {
    let composed = compose_ok(
        "template a {\n  dns \"192.168.50.182\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  dns \"192.168.50.183\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["192.168.50.182", "192.168.50.183"]);
}

/// ...and unlike the set-like lists, `dns` is *not* deduped (#69).
/// Ordering is observable there — it's resolver priority — so its append
/// semantics are left exactly as they were, even though a repeat is just
/// as meaningless as it is elsewhere.
#[test]
fn dns_keeps_duplicates() {
    let composed = compose_ok(
        "template a {\n  dns \"192.168.50.182\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  dns \"192.168.50.182\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["192.168.50.182", "192.168.50.182"]);
}

/// `env_file` is list-typed just like `dns` — it concatenates across
/// tiers rather than colliding (#154).
#[test]
fn env_file_concatenates_across_tiers() {
    let composed = compose_ok(
        "template a {\n  env_file \"common.env\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  env_file \"miniflux.env\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<&str> = service.fields.env_file.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["common.env", "miniflux.env"]);
}

/// ...and, like `dns`, `env_file` is *not* deduped: Compose's own
/// last-file-wins precedence for a variable set in two listed files
/// makes the order of a repeat observable, even a repeat naming the
/// exact same file twice.
#[test]
fn env_file_keeps_duplicates() {
    let composed = compose_ok(
        "template a {\n  env_file \"common.env\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  env_file \"common.env\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<&str> = service.fields.env_file.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["common.env", "common.env"]);
}

// --- privileged / devices (#157, map-kind since #167) ---

/// `devices` merges exactly like `publish` (#167) — keyed on the
/// container side, so non-colliding entries from different tiers
/// concatenate rather than colliding. Compare
/// `explicit_templates_publish_container_port_collision_is_error` for
/// the collision case this field now shares too.
#[test]
fn devices_accumulates_across_tiers() {
    let composed = compose_ok(
        "template a {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  devices \"/dev/fuse\" -> \"/dev/fuse\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<(&str, &str)> = service
        .fields
        .devices
        .entries
        .iter()
        .map(|e| (e.host.text(), e.container.text()))
        .collect();
    assert_eq!(
        entries,
        vec![("/dev/kmsg", "/dev/kmsg"), ("/dev/fuse", "/dev/fuse")]
    );
}

/// The service's own body always wins over an inherited template entry
/// for the same container path — `merge_map`'s ordinary Own-always-wins
/// rule, which happens to look like deduplication when the two entries
/// agree, exactly as it would for `publish`/`volume`.
#[test]
fn devices_own_body_overrides_template_for_same_container_path() {
    let composed = compose_ok(
        "template a {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.devices.entries.len(), 1);
}

/// Two *explicit* templates mapping different hosts onto the same
/// container path disagree about what that path should be — a compile
/// error, matching `publish`'s own
/// `explicit_templates_publish_container_port_collision_is_error`.
#[test]
fn explicit_templates_devices_container_path_collision_is_error() {
    let err = compose_err(
        "template a {\n  devices \"/dev/kmsg\" -> \"/dev/shared\"\n}\n\
         template b {\n  devices \"/dev/fuse\" -> \"/dev/shared\"\n}\n\
         service s {\n  with a, b\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "devices");
            assert_eq!(details.side, MapSide::Value);
            assert_eq!(details.key, "/dev/shared");
        }
        other => panic!("expected MapKeyCollision on devices, got {other:?}"),
    }
}

/// Non-colliding entries accumulate across tiers, and a `$param` in
/// either half of a mapping is substituted like any other literal
/// slot — the same live bug class issue #168 covers, and the same test
/// shape as `publish_accumulates_across_tiers_and_substitutes_params`.
#[test]
fn devices_accumulates_across_tiers_and_substitutes_params() {
    let composed = compose_ok(
        "template dev(d) {\n  devices $d -> $d\n}\n\
         service s {\n  with dev { d: \"/dev/kmsg\" }\n  devices \"/dev/fuse\" -> \"/dev/fuse\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<(&str, &str)> = service
        .fields
        .devices
        .entries
        .iter()
        .map(|e| (e.host.text(), e.container.text()))
        .collect();
    assert_eq!(
        entries,
        vec![("/dev/kmsg", "/dev/kmsg"), ("/dev/fuse", "/dev/fuse")]
    );
}

/// `privileged` is a bare-presence `ServiceFields` field merged via
/// `merge_scalar_like`, exactly like `healthcheck.disable` — same
/// Own-always-wins rule, exercised directly here rather than through
/// `healthcheck`'s nested struct. `Tier::Own` overwrites rather than
/// colliding, unlike two `Explicit` tiers setting the same field (see
/// `explicit_templates_setting_privileged_still_collide` below) — so
/// composing succeeds here specifically because the service's own body
/// is the tier doing the re-asserting.
#[test]
fn service_own_privileged_does_not_collide_with_an_explicit_templates_privileged() {
    let composed = compose_ok(
        "template needs_host_access {\n  privileged\n}\n\
         service cadvisor {\n  with needs_host_access\n  image \"x\"\n  privileged\n}\n",
    );
    let service = single_service(&composed);
    assert!(service.fields.privileged.is_some());
}

/// A `defaults` template setting `privileged` is silently overridden by
/// an explicit template's own `privileged` for the same field, mirroring
/// `defaults_healthcheck_test_loses_to_an_explicit_templates_test`'s
/// `(Tier::Defaults, _)` arm. `privileged`'s "value" is only ever
/// presence, so unlike `healthcheck.test`'s distinct strings, the two
/// tiers can't be told apart by content — only by which span composition
/// kept, so this pins that down by source line instead: `defaults`'s own
/// `privileged` sits on line 2, `real`'s on line 5, and it's `real`'s
/// that must survive.
#[test]
fn defaults_privileged_span_loses_to_an_explicit_templates_privileged() {
    let composed = compose_ok(
        "template defaults {\n  privileged\n}\n\
         template real {\n  privileged\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let span = service.fields.privileged.expect("privileged set");
    assert_eq!(span.line, 5);
}

/// Two explicit templates both setting `privileged` collide, the same
/// collision rule `healthcheck.disable` gets — see
/// `explicit_templates_setting_same_healthcheck_disable_still_collide`.
#[test]
fn explicit_templates_setting_privileged_still_collide() {
    let err = compose_err(
        "template a {\n  privileged\n}\n\
         template b {\n  privileged\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::FieldCollision {
                field: "privileged",
                ..
            }
        ),
        "got {err:?}"
    );
}

// --- cycles ---

#[test]
fn template_cycle_two_hop_is_error() {
    let err = compose_err(
        "template a {\n  with b\n}\n\
         template b {\n  with a\n}\n\
         service s {\n  with a\n}\n",
    );
    match err {
        ComposeError::TemplateCycle { chain, .. } => {
            assert_eq!(chain, vec!["a", "b", "a"]);
        }
        other => panic!("expected TemplateCycle, got {other:?}"),
    }
}

#[test]
fn template_cycle_three_hop_is_error() {
    let err = compose_err(
        "template a {\n  with b\n}\n\
         template b {\n  with c\n}\n\
         template c {\n  with a\n}\n\
         service s {\n  with a\n}\n",
    );
    match err {
        ComposeError::TemplateCycle { chain, .. } => {
            assert_eq!(chain, vec!["a", "b", "c", "a"]);
        }
        other => panic!("expected TemplateCycle, got {other:?}"),
    }
}

// --- `with` nesting depth (#72) ---

/// A linear chain `t0 <- t1 <- ... <- tN`, applied to a service via
/// `with tN`. Nothing repeats, so the cycle check never fires and this
/// is purely a depth question.
fn template_chain_source(n: usize) -> String {
    let mut source = String::from("template t0 {\n  image \"x\"\n}\n");
    for i in 1..=n {
        source.push_str(&format!("template t{i} {{\n  with t{}\n}}\n", i - 1));
    }
    source.push_str(&format!("service s {{\n  with t{n}\n}}\n"));
    source
}

/// A chain right at the ceiling still composes — the limit bounds depth,
/// it doesn't reject nesting.
#[test]
fn template_chain_at_the_depth_limit_composes() {
    let composed = compose_ok(&template_chain_source(hl_parser::MAX_TEMPLATE_DEPTH - 1));
    let service = single_service(&composed);
    assert_eq!(
        service
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

/// One level deeper is a catchable `ComposeError`. The cycle check can't
/// catch this — a linear chain repeats nothing — so before the depth
/// counter this recursed until the stack gave out and aborted the
/// process.
#[test]
fn template_chain_past_the_depth_limit_is_an_error() {
    let err = compose_err(&template_chain_source(hl_parser::MAX_TEMPLATE_DEPTH));
    assert!(matches!(
        err,
        ComposeError::TemplateNestingTooDeep { limit, .. } if limit == hl_parser::MAX_TEMPLATE_DEPTH
    ));
}

/// The depth the issue reproduced the abort at, well past where a
/// release build's stack gave out.
#[test]
fn a_pathologically_deep_template_chain_errors_instead_of_aborting() {
    let err = compose_err(&template_chain_source(20_000));
    assert!(matches!(err, ComposeError::TemplateNestingTooDeep { .. }));
}

/// A cycle is still reported as a cycle, not as a depth overflow — the
/// cycle check runs first, so the more specific diagnostic wins.
#[test]
fn a_cycle_is_still_a_cycle_not_a_depth_error() {
    let err = compose_err(
        "template a {\n  with b\n}\n\
         template b {\n  with a\n}\n\
         service s {\n  with a\n}\n",
    );
    assert!(matches!(err, ComposeError::TemplateCycle { .. }));
}

// --- unknown template / argument validation ---

#[test]
fn unknown_template_in_with_is_error() {
    let err = compose_err("service s {\n  with nonexistent\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnknownTemplate { name, .. } if name == "nonexistent"
    ));
}

#[test]
fn unknown_template_argument_is_error() {
    let err = compose_err(
        "template t(a) {\n  env A = $a\n}\n\
         service s {\n  with t { a: 1, b: 2 }\n}\n",
    );
    match err {
        ComposeError::UnknownTemplateArgument {
            template, argument, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(argument, "b");
        }
        other => panic!("expected UnknownTemplateArgument, got {other:?}"),
    }
}

#[test]
fn missing_template_argument_is_error() {
    let err = compose_err(
        "template t(a, b) {\n  env A = $a\n}\n\
         service s {\n  with t { a: 1 }\n}\n",
    );
    match err {
        ComposeError::MissingTemplateArgument {
            template, param, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "b");
        }
        other => panic!("expected MissingTemplateArgument, got {other:?}"),
    }
}

#[test]
fn duplicate_template_argument_is_error() {
    let err = compose_err(
        "template t(a) {\n  env A = $a\n}\n\
         service s {\n  with t { a: 1, a: 2 }\n}\n",
    );
    match err {
        ComposeError::DuplicateTemplateArgument {
            template, argument, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(argument, "a");
        }
        other => panic!("expected DuplicateTemplateArgument, got {other:?}"),
    }
}

#[test]
fn template_argument_not_scalar_is_error() {
    let err = compose_err(
        "template t(a) {\n  env A = $a\n}\n\
         service s {\n  with t { a: [1, 2] }\n}\n",
    );
    match err {
        ComposeError::TemplateArgumentNotScalar {
            template, param, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "a");
        }
        other => panic!("expected TemplateArgumentNotScalar, got {other:?}"),
    }
}

// --- parameter substitution and shape checking (#201) ---
//
// #201 dropped `: Number`/`: String` annotations in favor of checking a
// substituted argument against the *field's own* schema shape: a
// reference-shaped position (`networks`, `middleware`, `dns`,
// `env_file`, `depends_on`, `expose.entrypoint`, `router.entrypoint`,
// `router.path_prefix`, `router.middleware`) rejects a substituted
// `Literal::Number` — the
// one literal kind `parse_literal_reference` can never produce directly,
// so a `Number` reaching one of these fields can only mean a template
// caller passed a bare number where a reference belongs — while a
// `number`-typed position (`expose.port`, `healthcheck.retries`, per
// `book/src/built-in-fields.md`'s own "Accepts" column) rejects anything
// that isn't one, both when substituted and when written by hand. Every
// other scalar position (`container_name`, `restart.policy`, ...) takes
// any literal kind, exactly like writing it there directly would.

#[test]
fn reference_shaped_field_rejects_number_argument() {
    let err = compose_err(
        "template t(net) {\n  networks [$net]\n}\n\
         service s {\n  with t { net: 1000 }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotReferenceShaped {
            template, param, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "net");
        }
        other => panic!("expected ArgumentNotReferenceShaped, got {other:?}"),
    }
}

/// The rejected diagnostic points at the argument written at the `with`
/// call site, not at the `$net` reference inside the template body —
/// substitution overwrites the literal slot wholesale, span included, so
/// the span left behind is always the caller's own.
#[test]
fn reference_shaped_field_rejection_points_at_the_argument() {
    // Line 2 is the `$net` use site inside the template body; line 6 is
    // the `net: 1000` argument at the `with` call site. The diagnostic
    // must point at line 6, not line 2.
    let err = compose_err(
        "template t(net) {\n  networks [$net]\n}\n\
         service s {\n  with t {\n    net: 1000\n  }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotReferenceShaped { span, .. } => {
            assert_eq!(span.line, 6, "should point at the `1000` argument");
        }
        other => panic!("expected ArgumentNotReferenceShaped, got {other:?}"),
    }
}

/// An ordinary (non-numeric, non-reference-shaped) scalar field takes
/// whatever literal kind its argument happens to be, exactly like
/// writing it there directly would — `container_name`/`restart.policy`
/// aren't among `book/src/built-in-fields.md`'s `number`-typed rows, so
/// neither `numeric_mismatch` nor a reference-shape check ever runs
/// against them.
#[test]
fn scalar_field_accepts_any_literal_kind_via_param() {
    let composed = compose_ok(
        "template t(name, policy) {\n  \
           container_name $name\n  restart $policy\n\
         }\n\
         service s {\n  with t { name: 8080, policy: \"always\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service.fields.container_name.as_ref().unwrap().text(),
        "8080"
    );
    assert_eq!(
        service
            .fields
            .restart
            .as_ref()
            .unwrap()
            .policy
            .as_ref()
            .unwrap()
            .text(),
        "always"
    );
}

// --- numeric-field shape checking (#201's companion to the
// reference-shaped check above): `expose.port` and
// `healthcheck.retries` are `book/src/built-in-fields.md`'s two
// `number`-typed fields, so they get the same substitution-time
// replacement for the `: Number` annotation this issue dropped — plus a
// backstop that also catches a non-numeric literal written by hand, with
// no `$param`/template involved at all, which the substitution-time
// check alone can never see.

#[test]
fn numeric_field_rejects_non_numeric_argument() {
    let err = compose_err(
        "template t(port) {\n  expose $port\n}\n\
         service s {\n  with t { port: \"not-a-number\" }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotNumeric {
            template,
            param,
            found,
            ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "port");
            assert_eq!(found, "a quoted string");
        }
        other => panic!("expected ArgumentNotNumeric, got {other:?}"),
    }
}

/// Same span guarantee as the reference-shaped check: the diagnostic
/// points at the argument written at the `with` call site (line 6), not
/// the `$port` use site inside the template body (line 2).
#[test]
fn numeric_field_rejection_points_at_the_argument() {
    let err = compose_err(
        "template t(port) {\n  expose $port\n}\n\
         service s {\n  with t {\n    port: \"bad\"\n  }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotNumeric { span, .. } => {
            assert_eq!(span.line, 6, "should point at the `\"bad\"` argument");
        }
        other => panic!("expected ArgumentNotNumeric, got {other:?}"),
    }
}

#[test]
fn healthcheck_retries_rejects_non_numeric_argument() {
    let err = compose_err(
        "template t(tries) {\n  healthcheck { test: \"ok\"\n    retries: $tries }\n}\n\
         service s {\n  with t { tries: \"three\" }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotNumeric {
            template, param, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "tries");
        }
        other => panic!("expected ArgumentNotNumeric, got {other:?}"),
    }
}

/// The backstop: a non-numeric `expose.port` written directly, with no
/// template and no `$param` in sight, is rejected the same way a
/// substituted one is — the check that closes the gap a purely
/// substitution-time check would leave, since `substitute_numeric_literal`
/// only ever runs against a `Literal::Param` slot.
#[test]
fn hand_written_expose_port_rejects_a_non_numeric_literal() {
    let err = compose_err("service s {\n  image \"x\"\n  expose \"not-a-number\"\n}\n");
    match err {
        ComposeError::FieldNotNumeric { field, found, .. } => {
            assert_eq!(field, "expose.port");
            assert_eq!(found, "a quoted string");
        }
        other => panic!("expected FieldNotNumeric, got {other:?}"),
    }
}

/// Same backstop, for a template's own body rather than a plain
/// service's — no `$param` here either, just a mistake written directly
/// where a service using this template would otherwise never trigger
/// `substitute_numeric_literal` at all.
#[test]
fn hand_written_literal_inside_a_template_body_rejects_a_non_numeric_healthcheck_retries() {
    let err = compose_err(
        "template t {\n  healthcheck { test: \"ok\"\n    retries: \"three\" }\n}\n\
         service s {\n  with t\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::FieldNotNumeric { field, .. } => {
            assert_eq!(field, "healthcheck.retries");
        }
        other => panic!("expected FieldNotNumeric, got {other:?}"),
    }
}

/// Regression for the forwarding case `numeric_mismatch` has to defer
/// on: `outer`'s own `$x` isn't bound yet while `inner` is being
/// resolved as part of `outer`'s definition, so the literal substituted
/// into `expose.port` at that point is itself still `Literal::Param`,
/// not a concrete kind to judge — checking too early would have to
/// either wrongly accept a bad forwarded argument or wrongly reject a
/// good one. The mismatch still surfaces, correctly, once `outer` is
/// actually invoked with the bad argument.
#[test]
fn forwarded_numeric_param_is_checked_once_actually_bound() {
    let err = compose_err(
        "template inner(y) {\n  expose $y\n}\n\
         template outer(x) {\n  with inner { y: $x }\n}\n\
         service s {\n  with outer { x: \"bad\" }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotNumeric {
            template, param, ..
        } => {
            assert_eq!(template, "outer");
            assert_eq!(param, "x");
        }
        other => panic!("expected ArgumentNotNumeric, got {other:?}"),
    }
}

/// The mirror image of the regression above: a forwarded argument that
/// *is* a good number still composes cleanly all the way through.
#[test]
fn forwarded_numeric_param_composes_when_the_argument_is_a_number() {
    let composed = compose_ok(
        "template inner(y) {\n  expose $y\n}\n\
         template outer(x) {\n  with inner { y: $x }\n}\n\
         service s {\n  with outer { x: 8080 }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service
            .fields
            .expose
            .as_ref()
            .unwrap()
            .port
            .as_ref()
            .unwrap()
            .text(),
        "8080"
    );
}

#[test]
fn untyped_param_accepts_any_literal_kind() {
    let composed = compose_ok(
        "template t(x) {\n  env X = $x\n}\n\
         service s {\n  with t { x: bare-ident }\n  image \"i\"\n}\n",
    );
    let service = single_service(&composed);
    let value = &service.fields.env.entries[0].value;
    assert!(matches!(value, Literal::Ident(name, _) if name == "bare-ident"));
}

// --- nested template composition / parameter forwarding ---

#[test]
fn nested_template_composition_forwards_parameters() {
    let composed = compose_ok(
        "template inner(y) {\n  env Y = $y\n}\n\
         template outer(x) {\n  with inner { y: $x }\n}\n\
         service s {\n  with outer { x: 42 }\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.env.entries.len(), 1);
    assert_eq!(service.fields.env.entries[0].key.text(), "Y");
    let value = &service.fields.env.entries[0].value;
    assert_eq!(value.text(), "42");
    assert!(matches!(value, Literal::Number { .. }));
}

#[test]
fn template_param_is_substituted_inside_raw_scalar_list_and_map() {
    let composed = compose_ok(
        "template t(p) {\n  raw { plain: $p, items: [$p, \"x\"], nested: { k: $p } }\n}\n\
         service s {\n  with t { p: \"val\" }\n  image \"img\"\n}\n",
    );
    let service = single_service(&composed);
    let raw = &service.fields.raw.entries;
    assert_eq!(raw.len(), 3);

    let plain = raw.iter().find(|e| e.key.text() == "plain").unwrap();
    assert_eq!(raw_text(&plain.value), "val");

    let items = raw.iter().find(|e| e.key.text() == "items").unwrap();
    match &items.value {
        RawValue::List(elems, _) => {
            assert_eq!(elems.len(), 2);
            assert_eq!(raw_text(&elems[0]), "val");
            assert_eq!(raw_text(&elems[1]), "x");
        }
        other => panic!("expected a list raw value, got {other:?}"),
    }

    let nested = raw.iter().find(|e| e.key.text() == "nested").unwrap();
    match &nested.value {
        RawValue::Map(entries, _) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0.text(), "k");
            assert_eq!(raw_text(&entries[0].1), "val");
        }
        other => panic!("expected a map raw value, got {other:?}"),
    }
}

#[test]
fn composed_service_never_contains_unsubstituted_param() {
    let composed = compose_ok(
        "template inner(y) {\n  env Y = $y\n  expose $y\n  command $y\n}\n\
         template outer(x) {\n  with inner { y: $x }\n}\n\
         service s {\n  with outer { x: 42 }\n  image \"img\"\n}\n",
    );
    let service = single_service(&composed);
    assert_no_params(service);
}

/// `command`'s literals (#156) get substituted through the same
/// `substitute_params` walk as `restart.policy`/`expose.port` above,
/// for both the shell form and the exec form — see
/// `compose::substitute_params`'s own `command` case.
#[test]
fn command_param_is_substituted_in_shell_form() {
    let composed = compose_ok(
        "template t(cmd) {\n  command $cmd\n}\n\
         service s {\n  with t { cmd: \"npm start\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        command_shell_text(service.fields.command.as_ref().unwrap()),
        "npm start"
    );
}

/// A `$param` reference names a whole literal slot (docs/DESIGN.md's
/// lexical grammar — it's never string-interpolated inside a quoted
/// literal the way `{{name}}` is), so this substitutes a whole exec-list
/// item rather than a fragment inside one.
#[test]
fn command_param_is_substituted_in_exec_form() {
    let composed = compose_ok(
        "template t(arg) {\n  command [\"exec\", $arg]\n}\n\
         service s {\n  with t { arg: \"--user=miniflux\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    match service.fields.command.as_ref().unwrap() {
        Command::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(texts, vec!["exec", "--user=miniflux"]);
        }
        other => panic!("expected Command::Exec, got {other:?}"),
    }
}

/// Every literal-valued `healthcheck` sub-field (#153) goes through the
/// same `substitute_params` walk as `command` above (#168) — a `$param`
/// written into any of them used to survive composition unresolved and
/// reach codegen as the parameter's own name.
#[test]
fn healthcheck_params_are_substituted_in_every_literal_subfield() {
    let composed = compose_ok(
        "template checked(cmd, every, wait, tries, \
         grace, probe) {\n  \
           healthcheck {\n    \
             test: $cmd\n    \
             interval: $every\n    \
             timeout: $wait\n    \
             retries: $tries\n    \
             start_period: $grace\n    \
             start_interval: $probe\n  \
           }\n\
         }\n\
         service db {\n  image \"postgres:15\"\n  \
           with checked { cmd: \"pg_isready -U app\", every: \"10s\", wait: \"5s\", \
           tries: 3, grace: \"30s\", probe: \"2s\" }\n\
         }\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    assert_eq!(healthcheck_test_text(hc), "pg_isready -U app");
    assert_eq!(hc.interval.as_ref().unwrap().text(), "10s");
    assert_eq!(hc.timeout.as_ref().unwrap().text(), "5s");
    assert_eq!(hc.retries.as_ref().unwrap().text(), "3");
    assert_eq!(hc.start_period.as_ref().unwrap().text(), "30s");
    assert_eq!(hc.start_interval.as_ref().unwrap().text(), "2s");
    assert_no_params(service);
}

/// A `$param` reference names a whole literal slot, so an exec-form
/// `test` substitutes one list item at a time — the same shape as
/// `command_param_is_substituted_in_exec_form`.
#[test]
fn healthcheck_test_params_are_substituted_in_exec_form() {
    let composed = compose_ok(
        "template checked(bin, user) {\n  \
           healthcheck { test: [\"CMD\", $bin, \"-U\", $user] }\n\
         }\n\
         service db {\n  image \"postgres:15\"\n  \
           with checked { bin: \"pg_isready\", user: \"app\" }\n}\n",
    );
    let service = single_service(&composed);
    let hc = service
        .fields
        .healthcheck
        .as_ref()
        .expect("healthcheck set");
    match hc.test.as_ref().expect("test set") {
        HealthcheckTest::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(texts, vec!["CMD", "pg_isready", "-U", "app"]);
        }
        other => panic!("expected HealthcheckTest::Exec, got {other:?}"),
    }
    assert_no_params(service);
}

/// `entrypoint`'s literals (#183) get substituted through the same
/// `substitute_params` walk `command`'s do, in the shell form.
#[test]
fn entrypoint_param_is_substituted_in_shell_form() {
    let composed = compose_ok(
        "template t(ep) {\n  entrypoint $ep\n}\n\
         service s {\n  with t { ep: \"/entrypoint.sh\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        entrypoint_shell_text(service.fields.entrypoint.as_ref().unwrap()),
        "/entrypoint.sh"
    );
    assert_no_params(service);
}

/// A `$param` reference names a whole literal slot, so an exec-form
/// `entrypoint` substitutes one list item at a time — the same shape as
/// `command_param_is_substituted_in_exec_form`.
#[test]
fn entrypoint_param_is_substituted_in_exec_form() {
    let composed = compose_ok(
        "template t(arg) {\n  entrypoint [\"/bin/sh\", \"-c\", $arg]\n}\n\
         service s {\n  with t { arg: \"do-a-thing\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    match service.fields.entrypoint.as_ref().unwrap() {
        Entrypoint::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(texts, vec!["/bin/sh", "-c", "do-a-thing"]);
        }
        other => panic!("expected Entrypoint::Exec, got {other:?}"),
    }
    assert_no_params(service);
}

fn assert_no_params(service: &Service) {
    let fields = &service.fields;
    assert!(fields.with.is_empty(), "with should be fully resolved");
    if let Some(img) = &fields.image
        && let Some(r) = &img.reference
    {
        assert_not_param(r);
    }
    if let Some(e) = &fields.expose
        && let Some(p) = &e.port
    {
        assert_not_param(p);
    }
    if let Some(r) = &fields.restart
        && let Some(p) = &r.policy
    {
        assert_not_param(p);
    }
    for v in &fields.volumes.entries {
        // Only a bind-mount host can ever hold a `$param`: the parser
        // routes a `$` token to `BindMount`, never `Named` — see
        // `compose::substitute_params`'s own comment on this same walk
        // for why a named-volume host is unreachable here even post-#196.
        if let ArrowMapHost::BindMount(host) = &v.host {
            assert_not_param(host);
        }
        assert_not_param(&v.container);
    }
    for e in &fields.env.entries {
        assert_not_param(&e.key);
        assert_not_param(&e.value);
    }
    match &fields.command {
        Some(Command::Shell(lit)) => assert_not_param(lit),
        Some(Command::Exec(items, _)) => {
            for item in items {
                assert_not_param(item);
            }
        }
        None => {}
    }
    match &fields.entrypoint {
        Some(Entrypoint::Shell(lit)) => assert_not_param(lit),
        Some(Entrypoint::Exec(items, _)) => {
            for item in items {
                assert_not_param(item);
            }
        }
        None => {}
    }
    if let Some(hc) = &fields.healthcheck {
        match &hc.test {
            Some(HealthcheckTest::Shell(lit)) => assert_not_param(lit),
            Some(HealthcheckTest::Exec(items, _)) => {
                for item in items {
                    assert_not_param(item);
                }
            }
            None => {}
        }
        for lit in [
            hc.interval.as_ref(),
            hc.timeout.as_ref(),
            hc.retries.as_ref(),
            hc.start_period.as_ref(),
            hc.start_interval.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            assert_not_param(lit);
        }
    }
    for entry in &fields.raw.entries {
        assert_not_param(&entry.key);
        assert_raw_value_no_param(&entry.value);
    }
    // `router`'s three literal-holding sub-fields (#184/#196). Adding a
    // field without extending this walk is exactly what #168 was: the
    // `Literal::Param` survives composition and codegen emits the
    // parameter's own name into the generated document, silently and
    // with exit 0.
    for router in &fields.routers {
        if let Some(host) = &router.host {
            assert_not_param(host);
        }
        for entry in &router.entrypoint {
            assert_not_param(entry);
        }
        for prefix in &router.path_prefix {
            assert_not_param(prefix);
        }
    }
    // The reference-shaped list fields #196 opened to `$param` for the
    // first time — see `compose::substitute_params`'s own comment on
    // this same set of fields.
    for r in &fields.middleware {
        assert_not_param(r);
    }
    for r in &fields.networks {
        assert_not_param(r);
    }
    for r in &fields.dns {
        assert_not_param(r);
    }
    for r in &fields.env_file {
        assert_not_param(r);
    }
    for entry in &fields.depends_on {
        assert_not_param(&entry.reference);
    }
}

fn assert_not_param(lit: &Literal) {
    assert!(
        !matches!(lit, Literal::Param(_, _)),
        "found unsubstituted Literal::Param: {lit:?}"
    );
}

fn assert_raw_value_no_param(value: &RawValue) {
    match value {
        RawValue::Literal(lit) => assert_not_param(lit),
        RawValue::List(items, _) => {
            for item in items {
                assert_raw_value_no_param(item);
            }
        }
        RawValue::Map(entries, _) => {
            for (_, v) in entries {
                assert_raw_value_no_param(v);
            }
        }
    }
}

// --- template symbol table ---

#[test]
fn duplicate_top_level_template_name_is_error() {
    let err = compose_err(
        "template t {\n  image \"a\"\n}\n\
         template t {\n  image \"b\"\n}\n",
    );
    assert!(matches!(
        err,
        ComposeError::DuplicateTemplateName { name, .. } if name == "t"
    ));
}

/// #62: `defaults` is applied without an invocation, so nothing can
/// ever bind its parameters — declaring any is rejected rather than
/// silently leaking the parameter's own name (or panicking in codegen)
/// into the generated Compose file.
#[test]
fn parameterized_defaults_template_is_error() {
    let err = compose_err(
        "template defaults(x) {\n  restart $x\n}\n\
         service s {\n  image \"nginx\"\n}\n",
    );
    assert!(matches!(
        err,
        ComposeError::ParameterizedDefaults { param, .. } if param == "x"
    ));
}

/// The same rejection covers a parameter used in a `raw` block, which
/// is the shape that used to reach codegen's `raw` transcription.
#[test]
fn parameterized_defaults_template_with_raw_param_is_error() {
    let err = compose_err(
        "template defaults(x) {\n  raw { k: $x }\n}\n\
         service s {\n  image \"nginx\"\n}\n",
    );
    assert!(matches!(
        err,
        ComposeError::ParameterizedDefaults { param, .. } if param == "x"
    ));
}

/// A parameterless `defaults` is of course still fine — the rejection
/// keys on the parameter list, not on the name.
#[test]
fn parameterless_defaults_template_is_still_accepted() {
    let composed = compose_ok(
        "template defaults {\n  restart unless-stopped\n}\n\
         service s {\n  image \"nginx\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service
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
}

/// A *non*-`defaults` template may still declare parameters — this is
/// only about the implicitly-applied one.
#[test]
fn parameterized_non_defaults_template_is_unaffected() {
    let composed = compose_ok(
        "template based(policy) {\n  restart $policy\n}\n\
         service s {\n  with based { policy: \"always\" }\n  image \"nginx\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        service
            .fields
            .restart
            .as_ref()
            .unwrap()
            .policy
            .as_ref()
            .unwrap()
            .text(),
        "always"
    );
}

/// #63: the second `service web` used to silently replace the first,
/// dropping everything it declared (Traefik labels included) with no
/// diagnostic at all.
#[test]
fn duplicate_top_level_service_name_is_error() {
    let err = compose_err(
        "service web {\n  image \"a\"\n}\n\
         service web {\n  image \"b\"\n}\n",
    );
    assert!(matches!(
        err,
        ComposeError::DuplicateServiceName { name, .. } if name == "web"
    ));
}

/// #63: duplicate networks are worse than duplicate services — see
/// `ComposeError::DuplicateNetworkName`'s doc.
#[test]
fn duplicate_top_level_network_name_is_error() {
    let err = compose_err(
        "network proxy {\n  external\n}\n\
         network proxy {\n  name: \"other\"\n}\n",
    );
    assert!(matches!(
        err,
        ComposeError::DuplicateNetworkName { name, .. } if name == "proxy"
    ));
}

/// #60: a named volume is resolved by its bare name too, and its
/// declaration is what decides whether the volume is `external` or
/// carries a `name:` override — so two declarations under one name are
/// exactly as ambiguous as two networks are.
#[test]
fn duplicate_top_level_volume_name_is_error() {
    let err = compose_err("volume data {}\nvolume data {\n  external\n}\n");
    assert!(matches!(
        err,
        ComposeError::DuplicateVolumeName { name, .. } if name == "data"
    ));
}

/// Top-level `volume` declarations reach the composed program in source
/// order, for codegen to resolve named-volume mounts against.
#[test]
fn top_level_volumes_are_carried_through_composition() {
    let composed = compose_ok("volume a {}\nvolume b {\n  external\n}\n");
    let names: Vec<&str> = composed
        .volumes
        .iter()
        .map(|v| v.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
    assert!(composed.volumes[1].external.is_some());
}

/// A name may still be reused *across* declaration kinds — the symbol
/// tables are separate, and nothing looks a service up by a network's
/// or a volume's name or vice versa.
#[test]
fn same_name_across_different_declaration_kinds_is_fine() {
    let composed = compose_ok(
        "network shared {\n  external\n}\n\
         volume shared {}\n\
         template shared {\n  restart always\n}\n\
         service shared {\n  image \"x\"\n  with shared\n  networks [shared]\n  volume shared -> \"/data\"\n}\n",
    );
    assert_eq!(composed.networks.len(), 1);
    assert_eq!(composed.volumes.len(), 1);
    assert_eq!(composed.services.len(), 1);
}

// --- imports (Stage 1: no real file loading yet) ---

#[test]
fn qualified_network_reference_with_no_use_decls_is_unknown_alias() {
    let err = compose_err("service s {\n  image \"x\"\n  networks [traefik.traefik-net]\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnknownAlias { alias, .. } if alias == "traefik"
    ));
}

/// A qualified named-volume host answers the same way, for the same
/// reason: a lone `Program` has no imports, so no alias can be valid.
#[test]
fn qualified_volume_reference_with_no_use_decls_is_unknown_alias() {
    let err = compose_err("service s {\n  image \"x\"\n  volume storage.media -> \"/data\"\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnknownAlias { alias, .. } if alias == "storage"
    ));
}

/// A *bare* named-volume host is left completely untouched by
/// composition — it carries no qualifier, so there is nothing to resolve
/// until codegen looks it up against the program's declarations.
#[test]
fn bare_volume_reference_survives_composition_untouched() {
    let composed = compose_ok(
        "volume media {}\n\
         template mounts {\n  volume media -> \"/data\"\n}\n\
         service s {\n  image \"x\"\n  with mounts\n}\n",
    );
    let host = &composed.services[0].fields.volumes.entries[0].host;
    let ArrowMapHost::Named(r) = host else {
        panic!("expected a named-volume host, got {host:?}");
    };
    assert!(r.qualifier().is_none());
    assert_eq!(r.text(), "media");
}

#[test]
fn qualified_template_invocation_with_no_use_decls_is_unknown_alias() {
    let err = compose_err("service s {\n  with common.internal_web\n  image \"x\"\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnknownAlias { alias, .. } if alias == "common"
    ));
}

#[test]
fn qualified_middleware_reference_is_rejected() {
    let err = compose_err("service s {\n  image \"x\"\n  middleware [traefik.auth]\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnsupportedQualifiedReference { field: "middleware", alias, .. } if alias == "traefik"
    ));
}

#[test]
fn qualified_depends_on_reference_is_rejected() {
    let err = compose_err("service s {\n  image \"x\"\n  depends_on [other.db]\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnsupportedQualifiedReference { field: "depends_on", alias, .. } if alias == "other"
    ));
}

#[test]
fn qualified_dns_reference_is_rejected() {
    let err = compose_err("service s {\n  image \"x\"\n  dns [other.resolver]\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnsupportedQualifiedReference { field: "dns", alias, .. } if alias == "other"
    ));
}

#[test]
fn unqualified_middleware_depends_on_dns_are_accepted() {
    let composed = compose_ok(
        "service s {\n  image \"x\"\n  middleware [auth]\n  depends_on [db]\n  dns [resolver]\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.middleware.len(), 1);
    assert_eq!(service.fields.depends_on.len(), 1);
    assert_eq!(service.fields.dns.len(), 1);
}

/// A qualified `env_file` entry has no cross-file meaning — an `.env`
/// file lives on disk next to the compose file, not as a declaration any
/// `.hll` file could export — so it's rejected the same as `dns` (#154).
#[test]
fn qualified_env_file_reference_is_rejected() {
    let err = compose_err("service s {\n  image \"x\"\n  env_file [other.env]\n}\n");
    assert!(matches!(
        err,
        ComposeError::UnsupportedQualifiedReference { field: "env_file", alias, .. } if alias == "other"
    ));
}

#[test]
fn unqualified_env_file_is_accepted() {
    let composed = compose_ok("service s {\n  image \"x\"\n  env_file [\"miniflux.env\"]\n}\n");
    let service = single_service(&composed);
    assert_eq!(service.fields.env_file.len(), 1);
}

// --- `router` merge (#184) ---

fn router_named<'a>(service: &'a Service, name: &str) -> &'a hl_parser::Router {
    service
        .fields
        .routers
        .iter()
        .find(|r| r.key() == Some(name))
        .unwrap_or_else(|| panic!("no router named {name:?}"))
}

fn router_entrypoints(router: &hl_parser::Router) -> Vec<&str> {
    router.entrypoint.iter().map(|r| r.text()).collect()
}

fn router_prefixes(router: &hl_parser::Router) -> Vec<&str> {
    router.path_prefix.iter().map(Literal::text).collect()
}

fn router_middleware(router: &hl_parser::Router) -> Vec<&str> {
    router.middleware.iter().map(Literal::text).collect()
}

/// `router` merges keyed by name, then per sub-field within each name —
/// `expose`'s own per-sub-field merge, one level deeper. This is
/// `service_own_body_can_override_just_expose_host`'s exact scenario
/// applied to a router: the service replaces just `host` and still
/// inherits the template's `entrypoint` and `path_prefix` without
/// repeating them.
#[test]
fn service_own_body_can_override_just_one_router_subfield() {
    let composed = compose_ok(
        "template api_router {\n  \
           router api {\n    host: \"placeholder.example.com\"\n    \
             entrypoint: web-secure\n    path_prefix: [\"/api/v1\"]\n  }\n\
         }\n\
         service vikunja {\n  \
           with api_router\n  image \"vikunja/vikunja\"\n  \
           router api { host: \"vikunja.techdebtor.io\" }\n\
         }\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.routers.len(), 1);
    let api = router_named(service, "api");
    assert_eq!(api.host.as_ref().unwrap().text(), "vikunja.techdebtor.io");
    assert_eq!(router_entrypoints(api), vec!["web-secure"]);
    assert_eq!(router_prefixes(api), vec!["/api/v1"]);
}

/// Two *different* router names from two tiers are two routers, not a
/// collision — the keyed half of the merge.
#[test]
fn routers_with_different_names_accumulate_across_tiers() {
    let composed = compose_ok(
        "template defaults {\n  router lan, host: \"a.example.local\"\n}\n\
         template public {\n  router web, host: \"a.example.com\"\n}\n\
         service s {\n  with public\n  image \"x\"\n  router admin, host: \"admin.example.com\"\n}\n",
    );
    let service = single_service(&composed);
    let names: Vec<Option<&str>> = service.fields.routers.iter().map(|r| r.key()).collect();
    assert_eq!(names, vec![Some("lan"), Some("web"), Some("admin")]);
}

/// Two explicit templates disagreeing on one router's `host` is a
/// collision, reported with the router's own name — the same rule two
/// explicit templates setting `expose.host` already hit, keyed so the
/// message says *which* router.
#[test]
fn explicit_templates_setting_the_same_router_host_collide() {
    let err = compose_err(
        "template a {\n  router api, host: \"a.example.com\"\n}\n\
         template b {\n  router api, host: \"b.example.com\"\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::MapKeyCollision(details) => {
            assert_eq!(details.field, "router.host");
            assert_eq!(details.key, "api");
            assert_eq!(details.first_template, "a");
            assert_eq!(details.second_template, "b");
        }
        other => panic!("expected MapKeyCollision on router.host, got {other:?}"),
    }
}

/// ...but two explicit templates setting *different* sub-fields of one
/// router don't collide, exactly as they don't for `expose`.
#[test]
fn explicit_templates_setting_different_router_subfields_do_not_collide() {
    let composed = compose_ok(
        "template a {\n  router api, host: \"a.example.com\"\n}\n\
         template b {\n  router api, entrypoint: web-secure\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let api = router_named(service, "api");
    assert_eq!(api.host.as_ref().unwrap().text(), "a.example.com");
    assert_eq!(router_entrypoints(api), vec!["web-secure"]);
}

/// A router's `entrypoint` is set-like, so two tiers naming the same one
/// yield a router attached to it once — `expose.entrypoint`'s own
/// distinct-name rule.
#[test]
fn router_entrypoints_concatenate_and_dedupe() {
    let composed = compose_ok(
        "template a {\n  router api, entrypoint: web\n}\n\
         template b {\n  router api, entrypoint: web-secure\n}\n\
         service s {\n  with a, b\n  image \"x\"\n  \
           router api {\n    host: \"a.example.com\"\n    entrypoint: web-secure\n  }\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_entrypoints(router_named(service, "api")),
        vec!["web", "web-secure"]
    );
}

/// `path_prefix` concatenates *without* deduping, unlike `entrypoint`:
/// the entries are `||` alternatives whose written order is observable
/// in the emitted rule, the same reasoning that keeps `dns` and
/// `env_file` order-preserving.
#[test]
fn router_path_prefixes_concatenate_in_tier_order() {
    let composed = compose_ok(
        "template a {\n  router api, path_prefix: [\"/api/v1\"]\n}\n\
         service s {\n  with a\n  image \"x\"\n  \
           router api {\n    host: \"a.example.com\"\n    path_prefix: [\"/dav/\"]\n  }\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_prefixes(router_named(service, "api")),
        vec!["/api/v1", "/dav/"]
    );
}

/// `defaults` loses to an explicit template on a router's scalar
/// sub-field, silently, the way it loses everywhere else.
#[test]
fn defaults_router_host_is_overridden_silently() {
    let composed = compose_ok(
        "template defaults {\n  router api, host: \"placeholder.example.com\"\n}\n\
         template a {\n  router api, host: \"a.example.com\"\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_named(service, "api").host.as_ref().unwrap().text(),
        "a.example.com"
    );
}

/// A `$param` in a router's `host` is substituted like every other
/// literal slot — #168's bug class, which is a field added without
/// extending `substitute_params`' walk.
#[test]
fn router_host_param_is_substituted() {
    let composed = compose_ok(
        "template routed(h) {\n  router api { host: $h }\n}\n\
         service s {\n  with routed { h: \"a.example.com\" }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_named(service, "api").host.as_ref().unwrap().text(),
        "a.example.com"
    );
    assert_no_params(service);
}

/// And each `path_prefix` entry, which is why the field holds literals
/// rather than references: a reference has no `$param` form to
/// substitute at all.
#[test]
fn router_path_prefix_params_are_substituted() {
    let composed = compose_ok(
        "template routed(h, api, dav) {\n  \
           router api { host: $h\n    path_prefix: [$api, $dav] }\n\
         }\n\
         service s {\n  \
           with routed { h: \"a.example.com\", api: \"/api/v1\", dav: \"/dav/\" }\n  \
           image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let api = router_named(service, "api");
    assert_eq!(api.host.as_ref().unwrap().text(), "a.example.com");
    assert_eq!(router_prefixes(api), vec!["/api/v1", "/dav/"]);
    assert_no_params(service);
}

/// A router's entry point names something in the deployment's own
/// `traefik.yml`, not a declaration any `.hll` file exports, so a
/// qualifier has nothing to resolve against — rejected exactly as
/// `expose.entrypoint`'s is, rather than silently dropped on the way to
/// the label.
#[test]
fn qualified_router_entrypoint_reference_is_rejected() {
    let err = compose_err(
        "service s {\n  image \"x\"\n  router api, host: \"a.example.com\", entrypoint: traefik.web\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::UnsupportedQualifiedReference { field: "router.entrypoint", ref alias, .. } if alias == "traefik"
        ),
        "got {err:?}"
    );
}

/// The unnamed `router { }` form merges under its own key, distinct from
/// every named one — it isn't a wildcard that soaks up named blocks.
#[test]
fn unnamed_router_merges_under_its_own_key() {
    let composed = compose_ok(
        "template a {\n  router { entrypoint: web-secure }\n}\n\
         service s {\n  with a\n  image \"x\"\n  \
           router { host: \"a.example.com\" }\n  router api, host: \"b.example.com\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(service.fields.routers.len(), 2);
    let unnamed = service
        .fields
        .routers
        .iter()
        .find(|r| r.key().is_none())
        .expect("unnamed router kept");
    assert_eq!(unnamed.host.as_ref().unwrap().text(), "a.example.com");
    assert_eq!(router_entrypoints(unnamed), vec!["web-secure"]);
}

/// A service that never mentions `router` composes to an empty list, so
/// every file written before the field existed reaches codegen with
/// exactly the fields it always had.
#[test]
fn a_service_without_routers_composes_to_an_empty_list() {
    let composed = compose_ok(
        "template internal_web(port) {\n  expose $port\n  middleware auth\n}\n\
         service s {\n  with internal_web { port: 8080 }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert!(service.fields.routers.is_empty());
}

// --- per-router `middleware` (#221) ---

/// A router's own `middleware` merges across tiers exactly like its
/// `entrypoint`: concatenated in tier order and deduped by name, since a
/// repeat would be a repeated entry in the one comma-joined
/// `middlewares=` label. The *override* against the service-level field
/// is codegen's, not composition's — here the two simply coexist.
#[test]
fn router_middleware_concatenates_and_dedupes_across_tiers() {
    let composed = compose_ok(
        "template a {\n  router internal, middleware: local-ipwhitelist\n}\n\
         service s {\n  with a\n  image \"x\"\n  middleware forwardAuth-authentik\n  \
           router internal {\n    host: \"a.example.local\"\n    \
             middleware: [local-ipwhitelist, rate-limit]\n  }\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_middleware(router_named(service, "internal")),
        vec!["local-ipwhitelist", "rate-limit"]
    );
    // Untouched by the router's own list — the two are separate slots.
    let service_level: Vec<&str> = service
        .fields
        .middleware
        .iter()
        .map(Literal::text)
        .collect();
    assert_eq!(service_level, vec!["forwardAuth-authentik"]);
}

/// Two routers off one service carry independent lists — the whole
/// point of #221, and the shape `gitea.hll`'s public/internal pair needs.
#[test]
fn two_routers_keep_independent_middleware_lists() {
    let composed = compose_ok(
        "service gitea {\n  image \"x\"\n  \
           router public, host: \"git.example.com\"\n  \
           router internal {\n    host: \"git.internal.example.com\"\n    \
             middleware: local-ipwhitelist\n  }\n}\n",
    );
    let service = single_service(&composed);
    assert!(router_middleware(router_named(service, "public")).is_empty());
    assert_eq!(
        router_middleware(router_named(service, "internal")),
        vec!["local-ipwhitelist"]
    );
}

/// A `$param` in a router's `middleware` is substituted like every other
/// literal slot — #168's bug class, which is a field added without
/// extending `substitute_params`' walk.
#[test]
fn router_middleware_params_are_substituted() {
    let composed = compose_ok(
        "template routed(h, mw) {\n  router api { host: $h\n    middleware: [$mw] }\n}\n\
         service s {\n  \
           with routed { h: \"a.example.com\", mw: local-ipwhitelist }\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert_eq!(
        router_middleware(router_named(service, "api")),
        vec!["local-ipwhitelist"]
    );
    assert_no_params(service);
}

/// And it's reference-shaped, so a bare number substituted into it is
/// the same shape error every other reference position raises (#201).
#[test]
fn router_middleware_rejects_a_number_argument() {
    let err = compose_err(
        "template t(mw) {\n  router api { middleware: [$mw] }\n}\n\
         service s {\n  with t { mw: 1000 }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentNotReferenceShaped {
            template, param, ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "mw");
        }
        other => panic!("expected ArgumentNotReferenceShaped, got {other:?}"),
    }
}

/// A middleware lives in the deployment's own `traefik.yml`, not in a
/// declaration any `.hll` file exports, so a qualifier has nothing to
/// resolve against here either — rejected under the router's own field
/// path, so the diagnostic says which position was written.
#[test]
fn qualified_router_middleware_reference_is_rejected() {
    let err = compose_err(
        "service s {\n  image \"x\"\n  \
           router api, host: \"a.example.com\", middleware: traefik.forwardAuth\n}\n",
    );
    assert!(
        matches!(
            err,
            ComposeError::UnsupportedQualifiedReference { field: "router.middleware", ref alias, .. } if alias == "traefik"
        ),
        "got {err:?}"
    );
}
