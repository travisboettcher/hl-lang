use hl_parser::schema::MapSide;
use hl_parser::{Literal, ParseError, TemplateDecl, TopDecl, UseDecl, parse};

fn parse_ok(source: &str) -> hl_parser::Program {
    parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"))
}

fn as_service(decl: &TopDecl) -> &hl_parser::Service {
    match decl {
        TopDecl::Service(s) => s,
        other => panic!("expected a Service decl, got {other:?}"),
    }
}

fn as_network(decl: &TopDecl) -> &hl_parser::Network {
    match decl {
        TopDecl::Network(n) => n,
        other => panic!("expected a Network decl, got {other:?}"),
    }
}

fn as_template(decl: &TopDecl) -> &TemplateDecl {
    match decl {
        TopDecl::Template(t) => t,
        other => panic!("expected a Template decl, got {other:?}"),
    }
}

fn as_use(decl: &TopDecl) -> &UseDecl {
    match decl {
        TopDecl::Use(u) => u,
        other => panic!("expected a Use decl, got {other:?}"),
    }
}

// --- top level ---

#[test]
fn named_decl_requires_two_idents() {
    let err = parse("service {\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn unknown_top_level_type_is_error() {
    let err = parse("widget foo {}").unwrap_err();
    assert!(matches!(err, ParseError::UnknownTopLevelType { name, .. } if name == "widget"));
}

#[test]
fn program_with_multiple_top_decls() {
    let program = parse_ok("network a {}\nservice b {\n  image \"x\"\n}\n");
    assert_eq!(program.decls.len(), 2);
    assert!(matches!(program.decls[0], TopDecl::Network(_)));
    assert!(matches!(program.decls[1], TopDecl::Service(_)));
}

#[test]
fn empty_program_parses_to_empty_decls() {
    let program = parse_ok("");
    assert!(program.decls.is_empty());
}

// --- struct / primary shorthand ---

#[test]
fn image_primary_value_shorthand() {
    let program = parse_ok("service s {\n  image \"foo/bar:latest\"\n}\n");
    let service = as_service(&program.decls[0]);
    let image = service.fields.image.as_ref().unwrap();
    assert_eq!(image.reference.as_ref().unwrap().text(), "foo/bar:latest");
}

#[test]
fn image_canonical_body_form() {
    let program = parse_ok("service s {\n  image { ref: \"foo/bar:latest\" }\n}\n");
    let service = as_service(&program.decls[0]);
    let image = service.fields.image.as_ref().unwrap();
    assert_eq!(image.reference.as_ref().unwrap().text(), "foo/bar:latest");
}

#[test]
fn duplicate_image_field_is_error() {
    let err = parse("service s {\n  image \"a\"\n  image \"b\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "service",
            field: "image",
            ..
        }
    ));
}

#[test]
fn restart_primary_shorthand_bare_ident_policy() {
    let program = parse_ok("service s {\n  restart unless-stopped\n}\n");
    let service = as_service(&program.decls[0]);
    let restart = service.fields.restart.as_ref().unwrap();
    let policy = restart.policy.as_ref().unwrap();
    assert_eq!(policy.text(), "unless-stopped");
    assert!(matches!(policy, Literal::Ident(_, _)));
}

#[test]
fn restart_primary_shorthand_string_policy() {
    let program = parse_ok("service s {\n  restart \"unless-stopped\"\n}\n");
    let service = as_service(&program.decls[0]);
    let policy = service
        .fields
        .restart
        .as_ref()
        .unwrap()
        .policy
        .as_ref()
        .unwrap();
    assert_eq!(policy.text(), "unless-stopped");
    assert!(matches!(policy, Literal::Str(_, _)));
}

#[test]
fn unknown_struct_field_is_error() {
    let err = parse("service s {\n  bogus: \"x\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownField { type_name: "service", field, .. } if field == "bogus"
    ));
}

// --- expose / `as` alias ---

#[test]
fn expose_primary_only() {
    let program = parse_ok("service s {\n  expose 8096\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
    assert!(expose.host.is_none());
}

#[test]
fn expose_as_sugar_aliases_to_host() {
    let program = parse_ok("service s {\n  expose 8096 as \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.host.as_ref().unwrap().text(), "host.example.com");
}

#[test]
fn expose_host_explicit_field_form() {
    let program = parse_ok("service s {\n  expose 8096 host: \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.host.as_ref().unwrap().text(), "host.example.com");
}

#[test]
fn expose_duplicate_host_via_as_and_field_is_error() {
    let err = parse("service s {\n  expose 8096 as \"a\" host: \"b\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "expose",
            field: "host",
            ..
        }
    ));
}

// --- bool flag ---

#[test]
fn network_external_bare_flag() {
    let program = parse_ok("network n {\n  external\n}\n");
    let network = as_network(&program.decls[0]);
    assert!(network.external.is_some());
}

#[test]
fn network_without_external_defaults_false() {
    let program = parse_ok("network n {}\n");
    let network = as_network(&program.decls[0]);
    assert!(network.external.is_none());
}

#[test]
fn network_needs_name() {
    let err = parse("network {\n  external\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn network_real_name_field() {
    let program = parse_ok("network traefik-net {\n  external\n  name: \"docker_default\"\n}\n");
    let network = as_network(&program.decls[0]);
    assert_eq!(network.real_name.as_ref().unwrap().text(), "docker_default");
}

#[test]
fn network_without_real_name_field_is_none() {
    let program = parse_ok("network internal {}\n");
    let network = as_network(&program.decls[0]);
    assert!(network.real_name.is_none());
}

#[test]
fn expose_entrypoint_field() {
    let program = parse_ok("service s {\n  expose 8096 entrypoint: \"web-secure\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.entrypoint.as_ref().unwrap().text(), "web-secure");
}

#[test]
fn expose_without_entrypoint_field_is_none() {
    let program = parse_ok("service s {\n  expose 8096\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.expose.as_ref().unwrap().entrypoint.is_none());
}

#[test]
fn bool_flag_rejects_explicit_value() {
    let err = parse("network n {\n  external: true\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn bool_flag_duplicate_is_error() {
    // Regression test: a second bare `external` must be treated as
    // DuplicateField, not misread as an attempted value for the first
    // occurrence (a value-start token right after a bare flag is simply
    // the next statement, not part of the flag's own value — only `:`
    // is what makes a value 'attached' to the flag).
    let err = parse("network n {\n  external\n  external\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "network",
            field: "external",
            ..
        }
    ));
}

// --- maps: volume / env ---

#[test]
fn volume_arrow_sugar_bare_entry() {
    let program = parse_ok("service s {\n  volume \"host\" -> \"container\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(service.fields.volumes.entries[0].host.text(), "host");
    assert_eq!(
        service.fields.volumes.entries[0].container.text(),
        "container"
    );
}

#[test]
fn volume_colon_canonical_entry() {
    let program = parse_ok("service s {\n  volume { \"syncthing-config\": \"/config\" }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(
        service.fields.volumes.entries[0].host.text(),
        "syncthing-config"
    );
    assert_eq!(
        service.fields.volumes.entries[0].container.text(),
        "/config"
    );
}

#[test]
fn env_equals_sugar_bare_entry() {
    let program = parse_ok("service s {\n  env PUID = \"1000\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.env.entries.len(), 1);
    assert_eq!(service.fields.env.entries[0].key.text(), "PUID");
    assert_eq!(service.fields.env.entries[0].value.text(), "1000");
}

#[test]
fn env_repeated_entries_accumulate() {
    let program = parse_ok("service s {\n  env PUID = \"1000\"\n  env PGID = \"100\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.env.entries.len(), 2);
}

#[test]
fn env_duplicate_key_is_error() {
    let err = parse("service s {\n  env PUID = \"1000\"\n  env PUID = \"2000\"\n}\n").unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "env",
            side: MapSide::Key,
            value,
            ..
        } => {
            assert_eq!(value, "PUID");
        }
        other => panic!("expected DuplicateMapKey on env key, got {other:?}"),
    }
}

#[test]
fn volume_duplicate_container_path_is_error() {
    let err = parse("service s {\n  volume \"a\" -> \"/data\"\n  volume \"b\" -> \"/data\"\n}\n")
        .unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "volume",
            side: MapSide::Value,
            value,
            ..
        } => {
            assert_eq!(value, "/data");
        }
        other => panic!("expected DuplicateMapKey on volume container path, got {other:?}"),
    }
}

#[test]
fn volume_same_host_different_container_is_ok() {
    let program =
        parse_ok("service s {\n  volume \"a\" -> \"/data1\"\n  volume \"a\" -> \"/data2\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 2);
}

// --- raw ---

#[test]
fn raw_allows_arbitrary_keys() {
    let program = parse_ok("service s {\n  raw {\n    privileged: true\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 1);
    assert_eq!(service.fields.raw.entries[0].key.text(), "privileged");
}

#[test]
fn raw_preserves_nested_structure() {
    let program = parse_ok(
        "service s {\n  raw {\n    devices: [\"/dev/kmsg\"]\n    opts: { level: \"high\" }\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 2);
    match &service.fields.raw.entries[0].value {
        hl_parser::RawValue::List(items, _) => {
            assert_eq!(items.len(), 1);
            match &items[0] {
                hl_parser::RawValue::Literal(lit) => assert_eq!(lit.text(), "/dev/kmsg"),
                other => panic!("expected a literal list item, got {other:?}"),
            }
        }
        other => panic!("expected a list value, got {other:?}"),
    }
    match &service.fields.raw.entries[1].value {
        hl_parser::RawValue::Map(entries, _) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0.text(), "level");
        }
        other => panic!("expected a nested map value, got {other:?}"),
    }
}

#[test]
fn raw_no_uniqueness_check() {
    let program =
        parse_ok("service s {\n  raw {\n    key: \"a\"\n  }\n  raw {\n    key: \"b\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 2);
}

// --- reference lists ---

#[test]
fn middleware_repeats_accumulate() {
    let program = parse_ok("service s {\n  middleware a\n  middleware b\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .middleware
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn depends_on_bracket_list_form() {
    let program = parse_ok("service s {\n  depends_on [a, b]\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .depends_on
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn networks_comma_sugar_form() {
    let program = parse_ok("service s {\n  networks a, b\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .networks
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

// --- template declarations ---

#[test]
fn template_decl_with_body_parses() {
    let program = parse_ok("template t {\n  image \"x\"\n}\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(template.name.name, "t");
    assert!(template.params.is_empty());
    assert_eq!(
        template
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

#[test]
fn template_decl_empty_parens_same_as_no_parens() {
    let program = parse_ok("template t() {\n  image \"x\"\n}\n");
    let template = as_template(&program.decls[0]);
    assert!(template.params.is_empty());
}

#[test]
fn template_decl_with_params() {
    let program = parse_ok("template t(a, b) {\n  image \"x\"\n}\n");
    let template = as_template(&program.decls[0]);
    let names: Vec<&str> = template.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn template_decl_equals_shorthand() {
    let program = parse_ok("template t = restart unless-stopped\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(
        template
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
    assert!(template.fields.image.is_none());
}

#[test]
fn param_list_trailing_comma_is_error() {
    let err = parse("template t(a,) {\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn param_list_duplicate_param_is_error() {
    let err = parse("template t(a, a) {\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateTemplateParam { param, .. } if param == "a"
    ));
}

#[test]
fn unknown_field_in_template_body_reports_template_type_name() {
    let err = parse("template t {\n  bogus: \"x\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownField { type_name: "template", field, .. } if field == "bogus"
    ));
}

#[test]
fn literal_param_marks_only_matching_bare_idents_inside_own_template() {
    let program = parse_ok("template t(port) {\n  expose port\n}\n");
    let template = as_template(&program.decls[0]);
    let port = template
        .fields
        .expose
        .as_ref()
        .unwrap()
        .port
        .as_ref()
        .unwrap();
    assert!(matches!(port, Literal::Param(name, _) if name == "port"));
}

#[test]
fn literal_param_does_not_leak_into_unrelated_service() {
    // A service using the bare identifier "port" (unrelated to any
    // template's own parameter list) must still produce a plain
    // `Literal::Ident`, proving the parameter-marking pass never runs
    // outside a single template's own body.
    let program = parse_ok("service s {\n  restart port\n}\n");
    let service = as_service(&program.decls[0]);
    let policy = service
        .fields
        .restart
        .as_ref()
        .unwrap()
        .policy
        .as_ref()
        .unwrap();
    assert!(matches!(policy, Literal::Ident(name, _) if name == "port"));
}

// --- with ---

#[test]
fn with_bare_comma_list_with_args_parses() {
    let program = parse_ok("service s {\n  with internal_web { port: 8384 }, authenticated\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.with.len(), 2);
    assert_eq!(service.fields.with[0].name.name, "internal_web");
    assert_eq!(service.fields.with[0].args.entries.len(), 1);
    assert_eq!(service.fields.with[0].args.entries[0].key.text(), "port");
    assert_eq!(service.fields.with[1].name.name, "authenticated");
    assert!(service.fields.with[1].args.entries.is_empty());
}

#[test]
fn with_bracket_list_form_parses() {
    let program = parse_ok("service s {\n  with [a, b { x: 1 }]\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .with
        .iter()
        .map(|inv| inv.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn with_canonical_struct_form_parses() {
    let program = parse_ok("service s {\n  with { templates: [a, b] }\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .with
        .iter()
        .map(|inv| inv.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn with_zero_arg_invocation_has_empty_args() {
    let program = parse_ok("service s {\n  with authenticated\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.with.len(), 1);
    assert!(service.fields.with[0].args.entries.is_empty());
}

#[test]
fn with_second_occurrence_is_duplicate_field() {
    let err = parse("service s {\n  with a\n  with b\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "service",
            field: "with",
            ..
        }
    ));
}

#[test]
fn with_does_not_prevent_following_statement_from_parsing() {
    // Regression test for the syncthing worked example: `with a, b`
    // followed by `image "..."` on the next line must not swallow
    // `image` as part of the with-list.
    let program = parse_ok("service s {\n  with a, b\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.with.len(), 2);
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

// --- misc ---

#[test]
fn spans_are_retained_on_ast_nodes() {
    let program = parse_ok("service s {\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.span.start < service.span.end);
    let image = service.fields.image.as_ref().unwrap();
    assert!(image.span.start < image.span.end);
}

#[test]
fn unexpected_token_reports_position() {
    let err = parse("service s {\n  bogus\n}\n").unwrap_err();
    // "bogus" is an unknown field with no value/':' following, so the
    // parser reports it as UnknownField, not UnexpectedToken — assert the
    // reported position lands on line 2 (where "bogus" is).
    match err {
        ParseError::UnknownField { span, .. } => assert_eq!(span.line, 2),
        other => panic!("expected UnknownField, got {other:?}"),
    }
}

#[test]
fn number_literal_overflow_is_error() {
    let err = parse("service s {\n  expose 999999999999999999999999\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::NumberOutOfRange { .. }));
}

// --- use / qualified references ---

#[test]
fn use_decl_parses() {
    let program = parse_ok("use \"../docker/docker.hll\" as traefik\n");
    let u = as_use(&program.decls[0]);
    assert_eq!(u.path.text(), "../docker/docker.hll");
    assert_eq!(u.alias.name, "traefik");
}

#[test]
fn use_decl_missing_as_is_error() {
    let err = parse("use \"x.hll\" traefik\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn use_decl_requires_string_path() {
    // A bare/unquoted path isn't lexable as one token (IDENT can't
    // contain '.'/'/'), so this must fail expecting a STRING, not
    // silently accept "docker" as a (wrong) path.
    let err = parse("use docker as traefik\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn use_decl_missing_alias_is_error() {
    let err = parse("use \"x.hll\" as\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn program_with_use_and_service() {
    let program =
        parse_ok("use \"../docker/docker.hll\" as traefik\nservice s {\n  image \"x\"\n}\n");
    assert_eq!(program.decls.len(), 2);
    assert!(matches!(program.decls[0], TopDecl::Use(_)));
    assert!(matches!(program.decls[1], TopDecl::Service(_)));
}

#[test]
fn qualified_reference_in_networks_field() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks [traefik.traefik-net]\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert_eq!(r.qualifier.as_ref().unwrap().name, "traefik");
    assert_eq!(r.name, "traefik-net");
}

#[test]
fn unqualified_reference_has_no_qualifier() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks [traefik-net]\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert!(r.qualifier.is_none());
    assert_eq!(r.name, "traefik-net");
}

#[test]
fn qualified_reference_bare_comma_form() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks common.traefik-net\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert_eq!(r.qualifier.as_ref().unwrap().name, "common");
    assert_eq!(r.name, "traefik-net");
}

#[test]
fn qualified_template_invocation_in_with() {
    let program =
        parse_ok("service s {\n  with common.internal_web { port: 8384 }\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    let inv = &service.fields.with[0];
    assert_eq!(inv.qualifier.as_ref().unwrap().name, "common");
    assert_eq!(inv.name.name, "internal_web");
    assert_eq!(inv.args.entries.len(), 1);
}

#[test]
fn qualified_zero_arg_template_invocation() {
    let program = parse_ok("service s {\n  with common.authenticated\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    let inv = &service.fields.with[0];
    assert_eq!(inv.qualifier.as_ref().unwrap().name, "common");
    assert_eq!(inv.name.name, "authenticated");
    assert!(inv.args.entries.is_empty());
}

#[test]
fn qualified_middleware_reference_parses() {
    // Parsing accepts a qualified reference on any Reference-typed
    // field, including middleware/depends_on — compose() is what
    // rejects it as unsupported (schema-agnostic parser, per the
    // codebase's existing "don't special-case field identity in the
    // parser" precedent).
    let program = parse_ok("service s {\n  image \"x\"\n  middleware common.forwardAuth\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.middleware[0];
    assert_eq!(r.qualifier.as_ref().unwrap().name, "common");
    assert_eq!(r.name, "forwardAuth");
}
