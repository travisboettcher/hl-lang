use hl_parser::schema::MapSide;
use hl_parser::{Literal, ParseError, TopDecl, parse};

fn parse_ok(source: &str) -> hl_parser::Program {
    parse(source).unwrap_or_else(|err| panic!("unexpected parse error: {err}"))
}

fn as_service(decl: &TopDecl) -> &hl_parser::Service {
    match decl {
        TopDecl::Service(s) => s,
        TopDecl::Network(_) => panic!("expected a Service decl"),
    }
}

fn as_network(decl: &TopDecl) -> &hl_parser::Network {
    match decl {
        TopDecl::Network(n) => n,
        TopDecl::Service(_) => panic!("expected a Network decl"),
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
fn template_declaration_is_not_yet_supported_error() {
    let err = parse("template foo {}").unwrap_err();
    assert!(matches!(
        err,
        ParseError::TemplatesNotSupported {
            what: "template declaration",
            ..
        }
    ));
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
    let image = service.image.as_ref().unwrap();
    assert_eq!(image.reference.as_ref().unwrap().text(), "foo/bar:latest");
}

#[test]
fn image_canonical_body_form() {
    let program = parse_ok("service s {\n  image { ref: \"foo/bar:latest\" }\n}\n");
    let service = as_service(&program.decls[0]);
    let image = service.image.as_ref().unwrap();
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
    let restart = service.restart.as_ref().unwrap();
    let policy = restart.policy.as_ref().unwrap();
    assert_eq!(policy.text(), "unless-stopped");
    assert!(matches!(policy, Literal::Ident(_, _)));
}

#[test]
fn restart_primary_shorthand_string_policy() {
    let program = parse_ok("service s {\n  restart \"unless-stopped\"\n}\n");
    let service = as_service(&program.decls[0]);
    let policy = service.restart.as_ref().unwrap().policy.as_ref().unwrap();
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
    let expose = service.expose.as_ref().unwrap();
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
    assert!(expose.host.is_none());
}

#[test]
fn expose_as_sugar_aliases_to_host() {
    let program = parse_ok("service s {\n  expose 8096 as \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.expose.as_ref().unwrap();
    assert_eq!(expose.host.as_ref().unwrap().text(), "host.example.com");
}

#[test]
fn expose_host_explicit_field_form() {
    let program = parse_ok("service s {\n  expose 8096 host: \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.expose.as_ref().unwrap();
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
    assert_eq!(service.volumes.entries.len(), 1);
    assert_eq!(service.volumes.entries[0].host.text(), "host");
    assert_eq!(service.volumes.entries[0].container.text(), "container");
}

#[test]
fn volume_colon_canonical_entry() {
    let program = parse_ok("service s {\n  volume { \"syncthing-config\": \"/config\" }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.volumes.entries.len(), 1);
    assert_eq!(service.volumes.entries[0].host.text(), "syncthing-config");
    assert_eq!(service.volumes.entries[0].container.text(), "/config");
}

#[test]
fn env_equals_sugar_bare_entry() {
    let program = parse_ok("service s {\n  env PUID = \"1000\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.env.entries.len(), 1);
    assert_eq!(service.env.entries[0].key.text(), "PUID");
    assert_eq!(service.env.entries[0].value.text(), "1000");
}

#[test]
fn env_repeated_entries_accumulate() {
    let program = parse_ok("service s {\n  env PUID = \"1000\"\n  env PGID = \"100\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.env.entries.len(), 2);
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
    assert_eq!(service.volumes.entries.len(), 2);
}

// --- raw ---

#[test]
fn raw_allows_arbitrary_keys() {
    let program = parse_ok("service s {\n  raw {\n    privileged: true\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.raw.entries.len(), 1);
    assert_eq!(service.raw.entries[0].key.text(), "privileged");
}

#[test]
fn raw_preserves_nested_structure() {
    let program = parse_ok(
        "service s {\n  raw {\n    devices: [\"/dev/kmsg\"]\n    opts: { level: \"high\" }\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(service.raw.entries.len(), 2);
    match &service.raw.entries[0].value {
        hl_parser::RawValue::List(items, _) => {
            assert_eq!(items.len(), 1);
            match &items[0] {
                hl_parser::RawValue::Literal(lit) => assert_eq!(lit.text(), "/dev/kmsg"),
                other => panic!("expected a literal list item, got {other:?}"),
            }
        }
        other => panic!("expected a list value, got {other:?}"),
    }
    match &service.raw.entries[1].value {
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
    assert_eq!(service.raw.entries.len(), 2);
}

// --- reference lists ---

#[test]
fn middleware_repeats_accumulate() {
    let program = parse_ok("service s {\n  middleware a\n  middleware b\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service.middleware.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn depends_on_bracket_list_form() {
    let program = parse_ok("service s {\n  depends_on [a, b]\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service.depends_on.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn networks_comma_sugar_form() {
    let program = parse_ok("service s {\n  networks a, b\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service.networks.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

// --- with ---

#[test]
fn with_field_inside_service_is_not_yet_supported_error() {
    let err = parse("service s {\n  with foo\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::TemplatesNotSupported {
            what: "the `with` field",
            ..
        }
    ));
}

// --- misc ---

#[test]
fn spans_are_retained_on_ast_nodes() {
    let program = parse_ok("service s {\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.span.start < service.span.end);
    let image = service.image.as_ref().unwrap();
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
