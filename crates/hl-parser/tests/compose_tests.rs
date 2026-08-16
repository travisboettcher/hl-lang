//! Integration tests for template/`with` composition (`hl_parser::compose`),
//! covering docs/DESIGN.md's Composition section's merge/conflict rules
//! beyond the single canonical worked example already covered end-to-end
//! in `examples.rs`.

use hl_parser::schema::MapSide;
use hl_parser::{
    ComposeError, ComposedProgram, Expose, Literal, RawValue, Service, compose, parse,
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

// --- expose sub-field merge (#10) ---

fn entrypoints(expose: &Expose) -> Vec<&str> {
    expose.entrypoint.iter().map(|r| r.name.as_str()).collect()
}

/// The exact scenario from the issue report: a service overriding just
/// `expose.host` while still inheriting `port`/`entrypoint` from a
/// `with`-listed template, without repeating them.
#[test]
fn service_own_body_can_override_just_expose_host() {
    let composed = compose_ok(
        "template internal_web(port) {\n  \
           expose $port, entrypoint: \"web-secure\"\n\
         }\n\
         service it-tools {\n  \
           with internal_web { port: 8080 }\n  \
           image \"corentinth/it-tools:latest\"\n  \
           expose { host: \"tools.internal.techdebtor.io\" }\n\
         }\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8080");
    assert_eq!(
        expose.host.as_ref().unwrap().text(),
        "tools.internal.techdebtor.io"
    );
    assert_eq!(entrypoints(expose), vec!["web-secure"]);
}

/// Two explicit templates each setting a *different* `expose` sub-field
/// don't collide — only docs/DESIGN.md's "same field" collision rule
/// applies, and `port`/`host` are different fields now that `expose`
/// merges per sub-field instead of as one whole struct.
#[test]
fn explicit_templates_setting_different_expose_subfields_do_not_collide() {
    let composed = compose_ok(
        "template a {\n  expose 8080\n}\n\
         template b {\n  expose { host: \"x.example.com\" }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8080");
    assert_eq!(expose.host.as_ref().unwrap().text(), "x.example.com");
}

/// Two explicit templates setting the *same* `expose` sub-field still
/// collide — the per-sub-field merge narrows the granularity of the
/// existing collision rule, it doesn't remove it.
#[test]
fn explicit_templates_setting_same_expose_subfield_still_collide() {
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

/// An entry point names something in the deployment's own
/// `traefik.yml`, not a declaration any `.hll` file exports, so a
/// qualifier has nothing to resolve against — rejected rather than
/// silently dropped on the way to the label.
#[test]
fn qualified_entrypoint_reference_is_rejected() {
    let err = compose_err("service s {\n  image \"x\"\n  expose 80, entrypoint: traefik.web\n}\n");
    assert!(
        matches!(
            err,
            ComposeError::UnsupportedQualifiedReference { field: "expose.entrypoint", ref alias, .. } if alias == "traefik"
        ),
        "got {err:?}"
    );
}

/// `expose.entrypoint` is a reference list, so two explicit templates
/// both setting it *concatenate* rather than raising the
/// `FieldCollision` a scalar sub-field would — the same rule
/// `middleware` has always followed. This is the behavioral point of
/// making `entrypoint` a list: "attach this router to `web` and to
/// `web-secure`" is expressible by composition instead of by a
/// comma-in-a-string that codegen then has to tolerate.
#[test]
fn explicit_templates_both_setting_entrypoint_concatenate() {
    let composed = compose_ok(
        "template a {\n  expose { entrypoint: web }\n}\n\
         template b {\n  expose { entrypoint: web-secure }\n}\n\
         service s {\n  with a, b\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(entrypoints(expose), vec!["web", "web-secure"]);
}

/// And across all three tiers, in the same priority order every other
/// list field concatenates in.
#[test]
fn entrypoint_concatenates_across_all_three_tiers() {
    let composed = compose_ok(
        "template defaults {\n  expose { entrypoint: d }\n}\n\
         template a {\n  expose { entrypoint: t }\n}\n\
         service s {\n  with a\n  image \"x\"\n  expose { entrypoint: own }\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(entrypoints(expose), vec!["d", "t", "own"]);
}

/// `entrypoint` lives inside `expose`, which the merge rebuilds from
/// scratch — so an inherited entry point with no `port`/`host` beside
/// it still has to materialize the enclosing `Expose`, not vanish.
#[test]
fn entrypoint_alone_still_materializes_expose() {
    let composed = compose_ok(
        "template a {\n  expose { entrypoint: web }\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(entrypoints(expose), vec!["web"]);
    assert!(expose.port.is_none());
}

/// The mirror of the above: an `expose` with *no* entry points must not
/// be conjured into existence by the list merge, since an empty list
/// means "unset" (see `Expose::entrypoint`'s doc).
#[test]
fn no_entrypoint_anywhere_leaves_expose_unset() {
    let composed = compose_ok(
        "template a {\n  middleware auth\n}\n\
         service s {\n  with a\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    assert!(service.fields.expose.is_none());
}

/// `expose`'s span is stamped from `port` when one is present, even
/// though `entrypoint` came from an earlier tier — the scalar sub-field
/// table is applied before the list table precisely to keep that
/// preference order (see `ScalarField`'s doc).
#[test]
fn expose_span_prefers_port_over_entrypoint() {
    let composed = compose_ok(
        "template a {\n  expose { entrypoint: web }\n}\n\
         service s {\n  with a\n  image \"x\"\n  expose 8080\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    let port = expose.port.as_ref().expect("port set");
    assert_eq!(expose.span, port.span());
}

/// A `defaults` template setting one `expose` sub-field is silently
/// overridden only on that sub-field — the other sub-fields an explicit
/// template sets still come through, matching how `env`/`volume` map
/// entries already behave per-key.
#[test]
fn defaults_expose_subfield_is_overridden_but_others_survive() {
    let composed = compose_ok(
        "template defaults {\n  expose 1234, entrypoint: \"web\"\n}\n\
         template real {\n  expose 8080\n}\n\
         service s {\n  with real\n  image \"x\"\n}\n",
    );
    let service = single_service(&composed);
    let expose = service.fields.expose.as_ref().expect("expose set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8080");
    assert_eq!(entrypoints(expose), vec!["web"]);
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

#[test]
fn raw_concatenates_across_tiers_with_repeated_key_no_error() {
    let composed = compose_ok(
        "template a {\n  raw { key: \"a\" }\n}\n\
         template b {\n  raw { key: \"b\" }\n}\n\
         service s {\n  with a, b\n  raw { key: \"c\" }\n}\n",
    );
    let service = single_service(&composed);
    let values: Vec<&str> = service
        .fields
        .raw
        .entries
        .iter()
        .map(|e| raw_text(&e.value))
        .collect();
    assert_eq!(values, vec!["a", "b", "c"]);
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
    let names: Vec<&str> = service
        .fields
        .middleware
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, vec!["d1", "a1", "b1", "own1"]);
}

/// `dns` is list-typed just like `middleware`/`depends_on`/`networks` —
/// it concatenates across tiers rather than colliding.
#[test]
fn dns_concatenates_across_tiers() {
    let composed = compose_ok(
        "template a {\n  dns \"192.168.50.182\"\n}\n\
         service s {\n  with a\n  image \"x\"\n  dns \"192.168.50.183\"\n}\n",
    );
    let service = single_service(&composed);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(entries, vec!["192.168.50.182", "192.168.50.183"]);
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

// --- typed parameters ---

#[test]
fn number_typed_param_rejects_string_argument() {
    let err = compose_err(
        "template t(port: Number) {\n  expose $port\n}\n\
         service s {\n  with t { port: \"8080\" }\n}\n",
    );
    match err {
        ComposeError::ArgumentTypeMismatch {
            template,
            param,
            expected,
            ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "port");
            assert_eq!(expected, hl_parser::ParamType::Number);
        }
        other => panic!("expected ArgumentTypeMismatch, got {other:?}"),
    }
}

#[test]
fn string_typed_param_rejects_number_argument() {
    let err = compose_err(
        "template t(policy: String) {\n  restart $policy\n}\n\
         service s {\n  with t { policy: 1000 }\n  image \"x\"\n}\n",
    );
    match err {
        ComposeError::ArgumentTypeMismatch {
            template,
            param,
            expected,
            ..
        } => {
            assert_eq!(template, "t");
            assert_eq!(param, "policy");
            assert_eq!(expected, hl_parser::ParamType::String);
        }
        other => panic!("expected ArgumentTypeMismatch, got {other:?}"),
    }
}

#[test]
fn typed_param_accepts_matching_argument_kind() {
    let composed = compose_ok(
        "template t(port: Number, policy: String) {\n  \
           expose $port\n  restart $policy\n\
         }\n\
         service s {\n  with t { port: 8080, policy: \"always\" }\n  image \"x\"\n}\n",
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
        "template inner(y) {\n  env Y = $y\n  expose $y\n}\n\
         template outer(x) {\n  with inner { y: $x }\n}\n\
         service s {\n  with outer { x: 42 }\n  image \"img\"\n}\n",
    );
    let service = single_service(&composed);
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
    if let Some(e) = &fields.expose {
        if let Some(p) = &e.port {
            assert_not_param(p);
        }
        if let Some(h) = &e.host {
            assert_not_param(h);
        }
    }
    if let Some(r) = &fields.restart
        && let Some(p) = &r.policy
    {
        assert_not_param(p);
    }
    for v in &fields.volumes.entries {
        assert_not_param(&v.host);
        assert_not_param(&v.container);
    }
    for e in &fields.env.entries {
        assert_not_param(&e.key);
        assert_not_param(&e.value);
    }
    for entry in &fields.raw.entries {
        assert_not_param(&entry.key);
        assert_raw_value_no_param(&entry.value);
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
        "template defaults(x: String) {\n  restart $x\n}\n\
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
        "template defaults(x: Number) {\n  raw { k: $x }\n}\n\
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
        "template based(policy: String) {\n  restart $policy\n}\n\
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

/// A name may still be reused *across* declaration kinds — the three
/// symbol tables are separate, and nothing looks a service up by a
/// network's name or vice versa.
#[test]
fn same_name_across_different_declaration_kinds_is_fine() {
    let composed = compose_ok(
        "network shared {\n  external\n}\n\
         template shared {\n  restart always\n}\n\
         service shared {\n  image \"x\"\n  with shared\n  networks [shared]\n}\n",
    );
    assert_eq!(composed.networks.len(), 1);
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
