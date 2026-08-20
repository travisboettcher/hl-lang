use hl_lexer::TokenKind;
use hl_parser::schema::MapSide;
use hl_parser::{
    DependsOnCondition, Expected, Expose, HealthcheckTest, Literal, ParamType, ParseError,
    TemplateDecl, TopDecl, UseDecl, VolumeHost, parse,
};

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

fn as_volume(decl: &TopDecl) -> &hl_parser::Volume {
    match decl {
        TopDecl::Volume(v) => v,
        other => panic!("expected a Volume decl, got {other:?}"),
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
fn image_primary_value_shorthand_accepts_leading_colon() {
    let program = parse_ok("service s {\n  image: \"foo/bar:latest\"\n}\n");
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
fn image_with_no_value_and_no_brace_reports_expected_value_or_brace() {
    let err = parse("service s {\n  image\n}\n").unwrap_err();
    match err {
        ParseError::UnexpectedToken {
            expected,
            found_kind,
            ..
        } => {
            assert_eq!(expected, Expected::Description("a value or `{`"));
            assert_eq!(found_kind, TokenKind::RBrace);
        }
        other => panic!("expected UnexpectedToken, got {other:?}"),
    }
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
fn restart_primary_shorthand_accepts_leading_colon() {
    let program = parse_ok("service s {\n  restart: unless-stopped\n}\n");
    let service = as_service(&program.decls[0]);
    let restart = service.fields.restart.as_ref().unwrap();
    let policy = restart.policy.as_ref().unwrap();
    assert_eq!(policy.text(), "unless-stopped");
}

#[test]
fn unknown_struct_field_is_error() {
    let err = parse("service s {\n  bogus: \"x\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownField { type_name: "service", field, .. } if field == "bogus"
    ));
}

/// #84: whatever the unrecognized name is, the message points at the
/// `raw { ... }` passthrough — the workaround that already existed but
/// that this error never mentioned, turning a dead end into a one-line
/// fix for any Compose key `hll` has no field for yet.
#[test]
fn unknown_field_on_a_service_suggests_the_raw_escape_hatch() {
    let err = parse("service s {\n  cpu_shares: 512\n}\n").unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("raw { cpu_shares: ... }"),
        "expected the `raw` hint spelled with the offending field's own name, got: {message}"
    );
}

/// But only where a `raw` block would actually compile: `expose`'s own
/// body has no `raw` field, so suggesting one there would just be a
/// second error.
#[test]
fn unknown_field_on_a_nested_type_has_no_raw_hint() {
    let err = parse("service s {\n  expose { bogus: 1 }\n}\n").unwrap_err();
    let message = err.to_string();
    assert_eq!(message, "2:12: unknown field \"bogus\" on `expose`");
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
    let program = parse_ok("service s {\n  expose 8096, host: \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.host.as_ref().unwrap().text(), "host.example.com");
}

/// `as` is a one-shot fusion onto the primary value (no comma, no further
/// continuation) — a duplicate `host` can only be triggered through the
/// explicit comma-separated field form now, never by mixing `as` with a
/// trailing `host:`, since that combination no longer parses at all (see
/// `alias_sugar_cannot_be_followed_by_further_secondary_fields`).
#[test]
fn expose_duplicate_host_via_explicit_fields_is_error() {
    let err = parse("service s {\n  expose 8096, host: \"a\", host: \"b\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "expose",
            field: "host",
            ..
        }
    ));
}

/// `as` fuses onto the primary value as one self-contained unit (docs/
/// DESIGN.md's desugaring rule 3) — it cannot be followed by further
/// secondary fields, comma or no comma. If a service needs more than
/// `as` alone provides, it must use the explicit comma-separated field
/// form instead (`expose port, host: "...", entrypoint: "..."`) or the
/// canonical `expose { ... }` body.
///
/// #87: this dead end used to surface as a generic "expected a newline
/// before the next field, found Comma" from the *enclosing* body, never
/// mentioning the explicit field-list form that's the actual fix. It now
/// gets a dedicated error naming it directly.
#[test]
fn alias_sugar_cannot_be_followed_by_further_secondary_fields() {
    let err = parse(
        "service s {\n  expose 8096 as \"host.example.com\", entrypoint: \"web-secure\"\n}\n",
    )
    .unwrap_err();
    match err {
        ParseError::AliasSugarCannotContinue {
            type_name: "expose",
            keyword: "as",
            primary_field: "port",
            alias_field: "host",
            ..
        } => {}
        other => panic!("expected AliasSugarCannotContinue, got {other:?}"),
    }
    assert!(
        err.to_string().contains("expose <port>, host:"),
        "expected the canonical form in the message, got: {err}"
    );
}

// --- healthcheck (#153) ---

/// Every field set at once, in the canonical struct form.
#[test]
fn healthcheck_full_field_set() {
    let program = parse_ok(
        "service s {\n  \
           healthcheck {\n    \
             test: \"curl -f http://localhost\"\n    \
             interval: \"10s\"\n    \
             timeout: \"5s\"\n    \
             retries: 3\n    \
             start_period: \"30s\"\n    \
             start_interval: \"2s\"\n    \
             disable\n  \
           }\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    match hc.test.as_ref().unwrap() {
        HealthcheckTest::Shell(lit) => assert_eq!(lit.text(), "curl -f http://localhost"),
        other => panic!("expected HealthcheckTest::Shell, got {other:?}"),
    }
    assert_eq!(hc.interval.as_ref().unwrap().text(), "10s");
    assert_eq!(hc.timeout.as_ref().unwrap().text(), "5s");
    assert_eq!(hc.retries.as_ref().unwrap().text(), "3");
    assert_eq!(hc.start_period.as_ref().unwrap().text(), "30s");
    assert_eq!(hc.start_interval.as_ref().unwrap().text(), "2s");
    assert!(hc.disable.is_some());
}

/// The minimal case — one field set, everything else left `None` (never
/// enforced as required — see `ast::ServiceFields`'s doc).
#[test]
fn healthcheck_minimal_test_only() {
    let program = parse_ok("service s {\n  healthcheck { test: \"exit 0\" }\n}\n");
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    match hc.test.as_ref().unwrap() {
        HealthcheckTest::Shell(lit) => assert_eq!(lit.text(), "exit 0"),
        other => panic!("expected HealthcheckTest::Shell, got {other:?}"),
    }
    assert!(hc.interval.is_none());
    assert!(hc.disable.is_none());
}

/// A syntactically empty body must still parse — no field on
/// `Healthcheck` is enforced as required by the parser.
#[test]
fn healthcheck_empty_body_parses() {
    let program = parse_ok("service s {\n  healthcheck {}\n}\n");
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    assert!(hc.test.is_none());
    assert!(hc.disable.is_none());
}

/// The exec form: `test` as a bracketed list rather than a bare string.
#[test]
fn healthcheck_test_list_form() {
    let program = parse_ok(
        "service s {\n  \
           healthcheck {\n    \
             test: [\"CMD\", \"pg_isready\", \"-U\", \"miniflux\"]\n  \
           }\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    match hc.test.as_ref().unwrap() {
        HealthcheckTest::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(texts, vec!["CMD", "pg_isready", "-U", "miniflux"]);
        }
        other => panic!("expected HealthcheckTest::Exec, got {other:?}"),
    }
}

/// A `test` list of exactly one item is still the list form, not the
/// shell form — brackets are what select exec syntax, not item count.
#[test]
fn healthcheck_test_list_form_single_item() {
    let program = parse_ok("service s {\n  healthcheck { test: [\"CMD-SHELL\"] }\n}\n");
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    match hc.test.as_ref().unwrap() {
        HealthcheckTest::Exec(items, _) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "CMD-SHELL");
        }
        other => panic!("expected HealthcheckTest::Exec, got {other:?}"),
    }
}

/// `retries` is a plain number literal.
#[test]
fn healthcheck_retries_is_a_number() {
    let program = parse_ok("service s {\n  healthcheck { retries: 5 }\n}\n");
    let service = as_service(&program.decls[0]);
    let hc = service.fields.healthcheck.as_ref().unwrap();
    match hc.retries.as_ref().unwrap() {
        Literal::Number { value, .. } => assert_eq!(*value, 5),
        other => panic!("expected Literal::Number, got {other:?}"),
    }
}

/// `disable` is bare-presence only, exactly like `network`'s `external`
/// — a `:` after it is rejected rather than treated as an attempted
/// value.
#[test]
fn healthcheck_disable_rejects_a_colon_value() {
    let err = parse("service s {\n  healthcheck { disable: true }\n}\n").unwrap_err();
    assert!(
        matches!(err, ParseError::UnexpectedToken { .. }),
        "got {err:?}"
    );
}

/// `healthcheck` has no `primary_field` (see `schema::HEALTHCHECK`'s
/// doc) — unlike `expose`/`restart`/`image`, a bare value with no `{ }`
/// is rejected rather than silently meaning nothing in particular.
#[test]
fn healthcheck_bare_value_without_braces_is_rejected() {
    let err = parse("service s {\n  healthcheck \"exit 0\"\n}\n").unwrap_err();
    match err {
        ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::LBrace),
            ..
        } => {}
        other => panic!("expected UnexpectedToken expecting `{{`, got {other:?}"),
    }
}

/// Writing `test` twice is a duplicate-scalar compile error, same as
/// any other single-occurrence field.
#[test]
fn healthcheck_duplicate_test_is_error() {
    let err = parse("service s {\n  healthcheck {\n    test: \"a\"\n    test: \"b\"\n  }\n}\n")
        .unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateField {
                type_name: "healthcheck",
                field: "test",
                ..
            }
        ),
        "got {err:?}"
    );
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

// --- top-level `volume` declarations (#60) ---
//
// `volume` is the one identifier that names both a top-level
// declaration type and a `service`/`template` field. These pin that the
// two roles stay separate: the same word resolves through
// `schema::top_level_type` in one position and `schema::resolve_field`
// in the other, and neither leaks into the other's position.

#[test]
fn volume_decl_with_empty_body_parses() {
    let program = parse_ok("volume syncthing-config {}\n");
    let volume = as_volume(&program.decls[0]);
    assert_eq!(volume.name.name, "syncthing-config");
    assert!(volume.external.is_none());
    assert!(volume.real_name.is_none());
    assert!(volume.driver.is_none());
    assert!(volume.driver_opts.is_empty());
}

/// `external`/`name` are read exactly as `network`'s are — same field
/// names, same bare-flag/scalar kinds, same "unset means use the
/// declaration's own identifier" deferral.
#[test]
fn volume_decl_external_and_real_name() {
    let program = parse_ok("volume media {\n  external\n  name: \"media_store\"\n}\n");
    let volume = as_volume(&program.decls[0]);
    assert!(volume.external.is_some());
    assert_eq!(volume.real_name.as_ref().unwrap().text(), "media_store");
}

#[test]
fn volume_decl_driver_and_driver_opts() {
    let program = parse_ok(
        "volume backups {\n  \
           driver \"local\"\n  \
           driver_opts {\n    type: \"nfs\"\n    device: \":/exports/backups\"\n  }\n\
         }\n",
    );
    let volume = as_volume(&program.decls[0]);
    assert_eq!(volume.driver.as_ref().unwrap().text(), "local");
    let opts: Vec<(&str, &str)> = volume
        .driver_opts
        .iter()
        .map(|o| (o.key.text(), o.value.text()))
        .collect();
    assert_eq!(opts, vec![("type", "nfs"), ("device", ":/exports/backups")]);
}

#[test]
fn volume_decl_needs_name() {
    let err = parse("volume {\n  external\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn unknown_field_in_volume_decl_says_volume() {
    let err = parse("volume v {\n  nope: \"x\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownField {
            type_name: "volume",
            ref field,
            ..
        } if field == "nope"
    ));
}

/// The point of the whole two-roles arrangement: a top-level `volume`
/// declaration and a service-level `volume` *mount* in the same file
/// each parse as their own thing, in one parse.
#[test]
fn volume_decl_and_volume_field_coexist_in_one_file() {
    let program = parse_ok(
        "volume syncthing-config {}\n\
         service syncthing {\n  \
           image \"x\"\n  \
           volume syncthing-config -> \"/config\"\n\
         }\n",
    );
    assert_eq!(as_volume(&program.decls[0]).name.name, "syncthing-config");
    let service = as_service(&program.decls[1]);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(
        service.fields.volumes.entries[0].host.text(),
        "syncthing-config"
    );
}

/// The host side is split by *syntax*, not by the string's shape: an
/// unquoted identifier is a reference to a declaration, a quoted string
/// is a path. The two forms sit side by side in one body here so the
/// split can't be mistaken for a property of the content — `"media"`
/// would have been a named volume under the old leading-`/`-or-`.`
/// heuristic, and is unambiguously a bind mount now that it's quoted.
#[test]
fn volume_host_is_a_reference_when_unquoted_and_a_path_when_quoted() {
    let program = parse_ok(
        "service s {\n  \
           volume media -> \"/data\"\n  \
           volume \"media\" -> \"/other\"\n  \
           volume \"/mnt/x\" -> \"/x\"\n\
         }\n",
    );
    let entries = &as_service(&program.decls[0]).fields.volumes.entries;
    assert!(matches!(
        &entries[0].host,
        VolumeHost::Named(r) if r.name == "media" && r.qualifier.is_none()
    ));
    assert!(matches!(&entries[1].host, VolumeHost::BindMount(lit) if lit.text() == "media"));
    assert!(matches!(&entries[2].host, VolumeHost::BindMount(lit) if lit.text() == "/mnt/x"));
    // `VolumeHost`'s own accessors read through either arm, and each
    // host's span covers just that host — the entry span (which reaches
    // past the `->` to the container side) is a different, wider thing.
    let texts: Vec<&str> = entries.iter().map(|e| e.host.text()).collect();
    assert_eq!(texts, vec!["media", "media", "/mnt/x"]);
    for entry in entries {
        assert!(entry.host.span().end <= entry.span.end);
        assert_eq!(entry.host.span().start, entry.span.start);
    }
    // And the entry span really does reach past its own host, to the end
    // of the container side.
    assert!(entries[0].span.end > entries[0].host.span().end);
}

/// And a named-volume host takes the same `alias.name` qualifier every
/// other cross-file reference does — the parser records it; the linker
/// resolves it.
#[test]
fn volume_host_can_be_alias_qualified() {
    let program = parse_ok(
        "use \"shared.hll\" as common\n\
         service s {\n  volume common.media -> \"/data\"\n}\n",
    );
    let entries = &as_service(&program.decls[1]).fields.volumes.entries;
    let VolumeHost::Named(r) = &entries[0].host else {
        panic!("expected a named-volume host, got {:?}", entries[0].host);
    };
    assert_eq!(r.qualifier.as_ref().unwrap().name, "common");
    assert_eq!(r.name, "media");
}

/// The canonical map-body form takes both host kinds too, since it goes
/// through the same entry parser as the bare-entry sugar.
#[test]
fn volume_map_body_takes_both_host_kinds() {
    let program =
        parse_ok("service s {\n  volume {\n    media: \"/data\"\n    \"/mnt/x\": \"/x\"\n  }\n}\n");
    let entries = &as_service(&program.decls[0]).fields.volumes.entries;
    assert!(matches!(&entries[0].host, VolumeHost::Named(r) if r.name == "media"));
    assert!(matches!(&entries[1].host, VolumeHost::BindMount(_)));
}

/// A `publish` entry's key side stays a plain literal — the
/// reference-capable key is `volume`'s alone, so a bare identifier here
/// is still just a value.
#[test]
fn publish_keys_are_still_plain_literals() {
    let program = parse_ok("service s {\n  publish 8096 -> 8096\n}\n");
    let entries = &as_service(&program.decls[0]).fields.publish.entries;
    assert_eq!(entries[0].host.text(), "8096");
}

/// The service-level field keeps its map-kind bare-entry sugar — the
/// top-level declaration's struct-kind schema must not have displaced
/// it. A `volume` field written the way a declaration is written is
/// still a map entry missing its `->`.
#[test]
fn volume_field_still_requires_its_map_separator() {
    let err = parse("service s {\n  volume \"a\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::MapEntryMissingSeparator {
            type_name: "volume",
            separator: TokenKind::Arrow,
            ..
        }
    ));
}

/// And the reverse: a top-level `volume` body is a *struct* body, so a
/// map entry written there is a parse error rather than being silently
/// accepted as some map-kind sugar.
#[test]
fn map_entry_in_a_top_level_volume_body_is_error() {
    let err = parse("volume v {\n  \"a\" -> \"b\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownField {
            type_name: "volume",
            ..
        }
    ));
}

fn entrypoints(expose: &Expose) -> Vec<&str> {
    expose.entrypoint.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn expose_entrypoint_field() {
    let program = parse_ok("service s {\n  expose 8096, entrypoint: web-secure\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(entrypoints(expose), vec!["web-secure"]);
}

/// `entrypoint` is a reference list, spelled exactly like `middleware`:
/// a bare comma-separated list, a bracketed list, or a repeat of the
/// field, all of which accumulate.
#[test]
fn expose_entrypoint_accepts_a_bare_list() {
    let program =
        parse_ok("service s {\n  expose { port: 8096\n entrypoint: web, web-secure }\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(entrypoints(expose), vec!["web", "web-secure"]);
}

#[test]
fn expose_entrypoint_accepts_a_bracketed_list() {
    let program = parse_ok("service s {\n  expose 8096, entrypoint: [web, web-secure]\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(entrypoints(expose), vec!["web", "web-secure"]);
}

/// A quoted entry point is still a reference — `parse_key` accepts a
/// STRING — which is what keeps `entrypoint "{{name}}-secure"` (and
/// every pre-list config that quoted a single name) working.
#[test]
fn expose_entrypoint_accepts_a_quoted_name() {
    let program = parse_ok("service s {\n  expose 8096, entrypoint: \"web-secure\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(entrypoints(expose), vec!["web-secure"]);
}

/// `host`/`entrypoint` together, via the explicit comma-separated field
/// form rather than `as` — the shape `docs/DESIGN.md`'s `internal_web`
/// template now uses, since `as` can't be combined with anything else
/// (see `alias_sugar_cannot_be_followed_by_further_secondary_fields`).
/// Exercised inside a `template` body specifically, matching that real
/// worked example.
#[test]
fn expose_host_and_entrypoint_fields_in_template_body() {
    let program = parse_ok(
        "template internal_web(port: Number) {\n  \
           expose $port, host: \"{{name}}.internal.techdebtor.io\", entrypoint: \"web-secure\"\n  \
           middleware local-ipwhitelist\n\
         }\n",
    );
    let template = as_template(&program.decls[0]);
    let expose = template.fields.expose.as_ref().unwrap();
    assert_eq!(
        expose.host.as_ref().unwrap().text(),
        "{{name}}.internal.techdebtor.io"
    );
    assert_eq!(entrypoints(expose), vec!["web-secure"]);
    assert_eq!(template.fields.middleware.len(), 1);
}

/// A bare `entrypoint` list stops at the next `key:` rather than
/// swallowing it as another entry point — the one-token lookahead in
/// `parse_bare_reference_list`. Without it, `host` would be read as a
/// second entry point and the parse would then die on its `:` with an
/// error pointing at the wrong place entirely.
#[test]
fn bare_entrypoint_list_stops_at_the_next_field_key() {
    let program =
        parse_ok("service s {\n  expose 8096, entrypoint: web, host: \"x.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(entrypoints(expose), vec!["web"]);
    assert_eq!(expose.host.as_ref().unwrap().text(), "x.example.com");
}

/// The same lookahead must not over-trigger: a comma followed by a
/// plain reference (no colon after it) still continues the list.
#[test]
fn bare_middleware_list_still_continues_past_a_comma() {
    let program = parse_ok("service s {\n  middleware auth, cors\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .middleware
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, vec!["auth", "cors"]);
}

/// Two different fields joined by a comma, with no newline between them,
/// is now a hard error rather than being silently split into two
/// statements — `image` was never a field of `expose`, so this used to
/// parse as `expose 8096` followed by a separate `image "..."` statement;
/// now a comma may only ever continue the *same* statement's own value.
#[test]
fn different_fields_joined_by_comma_on_one_line_is_error() {
    let err = parse("service s {\n  expose 8096, image \"foo/bar:latest\"\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn expose_without_entrypoint_field_is_empty() {
    let program = parse_ok("service s {\n  expose 8096\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(
        service
            .fields
            .expose
            .as_ref()
            .unwrap()
            .entrypoint
            .is_empty()
    );
}

// --- container_name ---

#[test]
fn container_name_bare_shorthand() {
    let program = parse_ok("service s {\n  container_name \"uptime-kuma\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(
        service.fields.container_name.as_ref().unwrap().text(),
        "uptime-kuma"
    );
}

#[test]
fn container_name_colon_form() {
    let program = parse_ok("service s {\n  container_name: \"uptime-kuma\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(
        service.fields.container_name.as_ref().unwrap().text(),
        "uptime-kuma"
    );
}

#[test]
fn container_name_unset_is_none() {
    let program = parse_ok("service s {\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.container_name.is_none());
}

#[test]
fn duplicate_container_name_field_is_error() {
    let err =
        parse("service s {\n  container_name \"a\"\n  container_name \"b\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "service",
            field: "container_name",
            ..
        }
    ));
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
fn volume_bare_entry_accepts_leading_colon() {
    let program = parse_ok("service s {\n  volume: \"host\" -> \"container\"\n}\n");
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

/// #81: a comma between a map-kind body's entries is tolerated, same as
/// `raw {}` and a `with`-invocation's argument body — previously this
/// was a parse error (`expected a literal ..., found Comma`).
#[test]
fn volume_body_accepts_comma_between_entries() {
    let program = parse_ok("service s {\n  volume { \"a\": \"/x\", \"b\": \"/y\" }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 2);
    assert_eq!(service.fields.volumes.entries[0].host.text(), "a");
    assert_eq!(service.fields.volumes.entries[1].host.text(), "b");
}

/// #81 follow-up: same-line bare adjacency (no comma, no newline) between
/// two entries is a parse error, mirroring the comma-list rule the rest
/// of the language already follows elsewhere — a comma is never optional
/// when there's a next item, and now neither is a newline the comma
/// substitutes for.
#[test]
fn volume_body_rejects_bare_adjacency_on_one_line() {
    let err = parse("service s {\n  volume { \"a\": \"/x\" \"b\": \"/y\" }\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedToken {
            expected: Expected::Description("a comma or a newline before the next entry"),
            ..
        }
    ));
}

/// A newline between entries is still accepted with no comma needed —
/// only bare adjacency *on one line* is rejected.
#[test]
fn volume_body_accepts_a_newline_between_entries_with_no_comma() {
    let program =
        parse_ok("service s {\n  volume {\n    \"a\": \"/x\"\n    \"b\": \"/y\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 2);
}

/// Same rule applies to `raw {}`, which shares the same body-parsing
/// helper.
#[test]
fn raw_body_rejects_bare_adjacency_on_one_line() {
    let err = parse("service s {\n  raw { a: 1 b: 2 }\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedToken {
            expected: Expected::Description("a comma or a newline before the next entry"),
            ..
        }
    ));
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

/// #81: same optional-comma unification as `volume`'s own canonical body.
#[test]
fn env_body_accepts_comma_between_entries() {
    let program = parse_ok("service s {\n  env { PUID = \"1000\", PGID = \"100\" }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.env.entries.len(), 2);
    assert_eq!(service.fields.env.entries[0].key.text(), "PUID");
    assert_eq!(service.fields.env.entries[1].key.text(), "PGID");
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

/// #87: a `volume` entry missing its `->` used to blame whatever token
/// parsing stumbled on next — typically the start of the *following*
/// field, on a different line — rather than the entry itself. Reproduces
/// the issue's own repro: `volume "/data:/data"` (colons are just
/// ordinary string content inside the quotes, not a separator) with no
/// `-> "container"`, followed by an unrelated `env` field on the next
/// line.
#[test]
fn volume_entry_missing_separator_is_anchored_at_the_entry_not_the_next_field() {
    let err = parse(
        "service a {\n  image \"nginx\"\n  volume \"/data:/data\"\n  env TZ = \"America/Denver\"\n}\n",
    )
    .unwrap_err();
    match err {
        ParseError::MapEntryMissingSeparator {
            type_name: "volume",
            separator: TokenKind::Arrow,
            span,
        } => {
            assert_eq!(
                (span.line, span.col),
                (3, 10),
                "expected the error anchored at the volume entry itself (line 3), not the \
                 following env field (line 4)"
            );
        }
        other => panic!("expected MapEntryMissingSeparator, got {other:?}"),
    }
    assert!(
        err.to_string()
            .contains("`volume` entry has no `:` or `->`"),
        "expected the concrete separator token named in the message, got: {err}"
    );
}

#[test]
fn volume_same_host_different_container_is_ok() {
    let program =
        parse_ok("service s {\n  volume \"a\" -> \"/data1\"\n  volume \"a\" -> \"/data2\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 2);
}

// --- maps: publish (#84) ---

#[test]
fn publish_arrow_sugar_bare_entry() {
    let program = parse_ok("service s {\n  publish 8096 -> 8096\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.publish.entries.len(), 1);
    assert_eq!(service.fields.publish.entries[0].host.text(), "8096");
    assert_eq!(service.fields.publish.entries[0].container.text(), "8096");
}

#[test]
fn publish_bare_entry_accepts_leading_colon() {
    let program = parse_ok("service s {\n  publish: 8081 -> 80\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.publish.entries.len(), 1);
    assert_eq!(service.fields.publish.entries[0].host.text(), "8081");
    assert_eq!(service.fields.publish.entries[0].container.text(), "80");
}

#[test]
fn publish_colon_canonical_body() {
    let program = parse_ok("service s {\n  publish { 8384: 8384, 22000: 22000 }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.publish.entries.len(), 2);
    assert_eq!(service.fields.publish.entries[0].host.text(), "8384");
    assert_eq!(service.fields.publish.entries[1].container.text(), "22000");
}

/// Repeating the field accumulates rather than being a duplicate-scalar
/// error — the whole point of #84's "a service can only expose one".
#[test]
fn publish_repeats_accumulate() {
    let program = parse_ok("service s {\n  publish 8096 -> 8096\n  publish 8920 -> 8920\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.publish.entries.len(), 2);
}

/// A quoted side carries a protocol suffix (`53:53/udp` in Compose's own
/// short syntax) — which is also why uniqueness is checked on the
/// container side, so both protocols on one host port stay expressible.
#[test]
fn publish_accepts_a_quoted_protocol_suffix_on_the_container_side() {
    let program =
        parse_ok("service s {\n  publish 53 -> \"53/tcp\"\n  publish 53 -> \"53/udp\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.publish.entries.len(), 2);
    assert_eq!(service.fields.publish.entries[0].container.text(), "53/tcp");
    assert_eq!(service.fields.publish.entries[1].container.text(), "53/udp");
}

#[test]
fn publish_duplicate_container_port_is_error() {
    let err =
        parse("service s {\n  publish 8096 -> 8096\n  publish 8097 -> 8096\n}\n").unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "publish",
            side: MapSide::Value,
            value,
            ..
        } => assert_eq!(value, "8096"),
        other => panic!("expected DuplicateMapKey on publish container port, got {other:?}"),
    }
}

#[test]
fn publish_entry_missing_separator_is_an_error() {
    let err = parse("service s {\n  publish 8096\n}\n").unwrap_err();
    match err {
        ParseError::MapEntryMissingSeparator {
            type_name: "publish",
            separator: TokenKind::Arrow,
            span,
        } => assert_eq!((span.line, span.col), (2, 11)),
        other => panic!("expected MapEntryMissingSeparator, got {other:?}"),
    }
}

/// A `template` body accepts exactly the same fields as a `service` one.
#[test]
fn publish_is_accepted_in_a_template_body() {
    let program = parse_ok("template t {\n  publish 8096 -> 8096\n}\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(template.fields.publish.entries.len(), 1);
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
fn raw_accepts_leading_colon() {
    let program = parse_ok("service s {\n  raw: {\n    privileged: true\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 1);
    assert_eq!(service.fields.raw.entries[0].key.text(), "privileged");
}

#[test]
fn raw_allows_a_quoted_string_key() {
    let program = parse_ok("service s {\n  raw {\n    \"custom-key\": \"value\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 1);
    assert_eq!(service.fields.raw.entries[0].key.text(), "custom-key");
    assert!(matches!(
        service.fields.raw.entries[0].key,
        Literal::Str(_, _)
    ));
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

// --- statement separation: newline between struct-body fields ---

/// Two different fields in a struct-kind body (`service`/`template`/
/// `network`, or a nested type's own canonical `{ }` form) must be on
/// separate lines — this is the general form of
/// `different_fields_joined_by_comma_on_one_line_is_error` above, minus
/// the comma: no separator at all between two fields sharing a line is
/// just as invalid as a comma between them.
#[test]
fn two_fields_on_one_line_with_no_separator_is_error() {
    let err = parse("service s {\n  image \"x\" restart unless-stopped\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn two_fields_on_one_line_in_network_body_is_error() {
    let err = parse("network n {\n  external name: \"docker_default\"\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

/// A single-statement body needs nothing to separate — the newline
/// requirement only applies *between* two or more fields.
#[test]
fn single_statement_body_on_one_line_is_ok() {
    let program = parse_ok("service s { image \"x\" }\n");
    let service = as_service(&program.decls[0]);
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

/// Fields on separate lines remain valid with no separator at all —
/// this is the everyday case, confirming the newline requirement didn't
/// accidentally start requiring a comma too.
#[test]
fn fields_on_separate_lines_need_no_comma() {
    let program = parse_ok("service s {\n  image \"x\"\n  restart unless-stopped\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.image.is_some());
    assert!(service.fields.restart.is_some());
}

/// Map/raw-kind bodies (here, a `with`-invocation's own argument body,
/// which reuses `raw`'s entry parsing) are *not* struct-kind bodies, so
/// the newline-between-fields rule doesn't apply to them — the
/// compact, comma-separated one-liner style (`{ puid: 1000, pgid: 100
/// }`) used throughout docs/DESIGN.md's worked examples stays valid.
#[test]
fn with_invocation_argument_body_keeps_compact_comma_style() {
    let program = parse_ok(
        "service s {\n  with linuxserver_app { puid: 1000, pgid: 100 }\n  image \"x\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let inv = &service.fields.with[0];
    assert_eq!(inv.args.entries.len(), 2);
}

/// Same exemption for `raw`'s own body.
#[test]
fn raw_body_keeps_compact_comma_style() {
    let program = parse_ok("service s {\n  raw { key1: \"a\", key2: \"b\" }\n}\n");
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
        .map(|e| e.reference.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
    assert!(
        service
            .fields
            .depends_on
            .iter()
            .all(|e| e.condition.is_none())
    );
}

/// `db { condition: service_healthy }` (#155): the bracketed extended
/// form carries its condition alongside the plain reference.
#[test]
fn depends_on_extended_form_parses_the_condition() {
    let program = parse_ok("service s {\n  depends_on [db { condition: service_healthy }]\n}\n");
    let service = as_service(&program.decls[0]);
    let entry = &service.fields.depends_on[0];
    assert_eq!(entry.reference.name, "db");
    assert_eq!(
        entry.condition.map(|(c, _)| c),
        Some(DependsOnCondition::ServiceHealthy)
    );
}

/// The extended form also works unbracketed, as the bare single-item
/// sugar every `depends_on` entry gets.
#[test]
fn depends_on_extended_form_works_without_brackets() {
    let program = parse_ok("service s {\n  depends_on db { condition: service_healthy }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.depends_on.len(), 1);
    assert_eq!(
        service.fields.depends_on[0].condition.map(|(c, _)| c),
        Some(DependsOnCondition::ServiceHealthy)
    );
}

/// A mixed list — a plain reference alongside a conditioned one, in
/// either order — parses each entry independently.
#[test]
fn depends_on_mixed_bare_and_conditioned_entries_parse() {
    let program =
        parse_ok("service s {\n  depends_on [cache, db { condition: service_healthy }]\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.depends_on.len(), 2);
    assert_eq!(service.fields.depends_on[0].reference.name, "cache");
    assert!(service.fields.depends_on[0].condition.is_none());
    assert_eq!(service.fields.depends_on[1].reference.name, "db");
    assert_eq!(
        service.fields.depends_on[1].condition.map(|(c, _)| c),
        Some(DependsOnCondition::ServiceHealthy)
    );
}

/// All three of Compose's own condition values are accepted.
#[test]
fn depends_on_all_three_condition_values_parse() {
    for (text, expected) in [
        ("service_started", DependsOnCondition::ServiceStarted),
        ("service_healthy", DependsOnCondition::ServiceHealthy),
        (
            "service_completed_successfully",
            DependsOnCondition::ServiceCompletedSuccessfully,
        ),
    ] {
        let source = format!("service s {{\n  depends_on [db {{ condition: {text} }}]\n}}\n");
        let program = parse_ok(&source);
        let service = as_service(&program.decls[0]);
        assert_eq!(
            service.fields.depends_on[0].condition.map(|(c, _)| c),
            Some(expected),
            "condition {text:?} did not round-trip"
        );
    }
}

/// A condition outside Compose's own three fixed values is a compile
/// error naming all three legal ones.
#[test]
fn depends_on_invalid_condition_is_error() {
    let err = parse("service s {\n  depends_on [db { condition: service_ok }]\n}\n")
        .expect_err("expected a parse error");
    assert!(matches!(
        err,
        ParseError::InvalidDependsOnCondition { found, .. } if found == "service_ok"
    ));
}

/// `condition` is the only legal key inside a `depends_on` entry's body
/// — anything else is `UnknownField`, same as any other struct-shaped
/// body.
#[test]
fn depends_on_entry_unknown_key_is_error() {
    let err = parse("service s {\n  depends_on [db { bogus: 1 }]\n}\n")
        .expect_err("expected a parse error");
    assert!(matches!(
        err,
        ParseError::UnknownField { type_name: "depends_on", field, .. } if field == "bogus"
    ));
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

#[test]
fn dns_bracket_list_form() {
    let program = parse_ok("service s {\n  dns [\"192.168.50.182\"]\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(entries, vec!["192.168.50.182"]);
}

#[test]
fn dns_repeats_accumulate() {
    let program = parse_ok("service s {\n  dns \"192.168.50.182\"\n  dns \"192.168.50.183\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(entries, vec!["192.168.50.182", "192.168.50.183"]);
}

/// `env_file "one.env"` — the bare single-item sugar every reference-list
/// field gets for free (#154).
#[test]
fn env_file_bare_single_form() {
    let program = parse_ok("service s {\n  env_file \"miniflux.env\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service
        .fields
        .env_file
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(entries, vec!["miniflux.env"]);
}

#[test]
fn env_file_bracket_list_form() {
    let program = parse_ok("service s {\n  env_file [\"miniflux.env\", \"common.env\"]\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service
        .fields
        .env_file
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(entries, vec!["miniflux.env", "common.env"]);
}

#[test]
fn env_file_repeats_accumulate() {
    let program =
        parse_ok("service s {\n  env_file \"miniflux.env\"\n  env_file \"common.env\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service
        .fields
        .env_file
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(entries, vec!["miniflux.env", "common.env"]);
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
    let names: Vec<&str> = template
        .params
        .iter()
        .map(|p| p.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
    assert!(template.params.iter().all(|p| p.ty.is_none()));
}

#[test]
fn template_decl_with_typed_params() {
    let program = parse_ok("template t(a: Number, b: String) {\n  image \"x\"\n}\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(template.params[0].ty, Some(ParamType::Number));
    assert_eq!(template.params[1].ty, Some(ParamType::String));
}

#[test]
fn param_list_unknown_type_is_error() {
    let err = parse("template t(a: Boolean) {\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownParamType { name, .. } if name == "Boolean"
    ));
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
fn dollar_param_reference_resolves_inside_own_template() {
    let program = parse_ok("template t(port) {\n  expose $port\n}\n");
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
fn bare_ident_inside_template_body_is_never_treated_as_a_param() {
    // Without the `$` sigil, a bare identifier that happens to match a
    // declared parameter's name is still an ordinary `Literal::Ident` —
    // resolution is driven entirely by the sigil now, never by a
    // name-matching heuristic.
    let program = parse_ok("template t(port) {\n  restart port\n}\n");
    let template = as_template(&program.decls[0]);
    let policy = template
        .fields
        .restart
        .as_ref()
        .unwrap()
        .policy
        .as_ref()
        .unwrap();
    assert!(matches!(policy, Literal::Ident(name, _) if name == "port"));
}

#[test]
fn literal_param_does_not_leak_into_unrelated_service() {
    // A service using the bare identifier "port" (unrelated to any
    // template's own parameter list) must still produce a plain
    // `Literal::Ident`.
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

#[test]
fn dollar_reference_outside_template_body_is_error() {
    let err = parse("service s {\n  restart $port\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::ParamReferenceOutsideTemplate { name, .. } if name == "port"
    ));
}

#[test]
fn dollar_reference_to_undeclared_param_is_error() {
    let err = parse("template t(port) {\n  expose $prot\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownTemplateParam { name, .. } if name == "prot"
    ));
}

#[test]
fn dollar_reference_forwarded_through_nested_with_invocation() {
    // `$x` inside a template's own `with`-invocation argument body
    // resolves against that same enclosing template's declared params —
    // parameter forwarding, e.g. `template outer(x) { with inner { y: $x } }`.
    let program = parse_ok("template outer(x) {\n  with inner { y: $x }\n}\n");
    let template = as_template(&program.decls[0]);
    let inv = &template.fields.with[0];
    let value = &inv.args.entries[0].value;
    assert!(matches!(
        value,
        hl_parser::RawValue::Literal(Literal::Param(name, _)) if name == "x"
    ));
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

/// docs/DESIGN.md notes that a trailing comma continues a comma-list
/// across lines, so a long `with` list can be wrapped for readability —
/// see its worked-examples section. Checked here against the same
/// invocation list as `with_bare_comma_list_with_args_parses`, just
/// split across lines, to confirm the two forms parse identically.
#[test]
fn with_bare_comma_list_across_multiple_lines_parses_same_as_one_line() {
    let program =
        parse_ok("service s {\n  with internal_web { port: 8384 },\n       authenticated\n}\n");
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
fn with_canonical_struct_form_bare_list_parses() {
    let program = parse_ok("service s {\n  with { templates: a, b }\n}\n");
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
fn use_decl_wrong_keyword_instead_of_as_is_error() {
    // A real ident that isn't literally `as` right after the path must
    // still error — not be silently accepted as if it were `as` just
    // because a further, unrelated ident happens to follow it (which
    // would otherwise parse as a plausible-looking alias).
    let err = parse("use \"x.hll\" typo alias\n").unwrap_err();
    match err {
        ParseError::UnexpectedToken {
            expected,
            found_kind,
            found_lexeme,
            ..
        } => {
            assert_eq!(expected, Expected::Description("`as`"));
            assert_eq!(found_kind, TokenKind::Ident);
            assert_eq!(found_lexeme, "typo");
        }
        other => panic!("expected UnexpectedToken, got {other:?}"),
    }
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

// --- raw nesting depth (#72) ---

/// Wraps `k: <value>` in `n` nested `[ ]`, the shape the issue used to
/// overflow the stack with.
fn nested_raw_source(n: usize) -> String {
    format!(
        "service s {{\n  image \"x\"\n  raw {{ k: {}{} }}\n}}\n",
        "[".repeat(n),
        "]".repeat(n)
    )
}

/// The limit is a real ceiling, not a rejection of anything nested: a
/// `raw` value right at it still parses.
#[test]
fn raw_value_at_the_depth_limit_parses() {
    let program = parse_ok(&nested_raw_source(hl_parser::MAX_RAW_VALUE_DEPTH));
    assert_eq!(as_service(&program.decls[0]).fields.raw.entries.len(), 1);
}

/// ...and dropping that maximally deep tree has to be safe too, since
/// drop glue recurses just like the parser did and `Drop` can't return
/// an error. This is the half of the fix a bare depth counter with too
/// generous a limit would still get wrong, so it's asserted explicitly
/// rather than left to the end of the enclosing scope.
#[test]
fn a_raw_value_at_the_depth_limit_drops_without_overflowing() {
    let program = parse_ok(&nested_raw_source(hl_parser::MAX_RAW_VALUE_DEPTH));
    drop(program);
}

/// One level past the limit is a catchable `ParseError`, not the
/// `fatal runtime error: stack overflow` process abort it used to be —
/// which matters because these crates are a library with public
/// `parse()`/`link()` entry points an embedder can't defend behind.
#[test]
fn raw_value_past_the_depth_limit_is_an_error() {
    let err = parse(&nested_raw_source(hl_parser::MAX_RAW_VALUE_DEPTH + 1))
        .expect_err("expected a parse error");
    assert!(matches!(
        err,
        ParseError::RawValueTooDeep { limit, .. } if limit == hl_parser::MAX_RAW_VALUE_DEPTH
    ));
}

/// The depth that used to abort the process outright — tens of thousands
/// of levels, well past where even a release build's stack gave out.
#[test]
fn a_pathologically_deep_raw_value_errors_instead_of_aborting() {
    let err = parse(&nested_raw_source(50_000)).expect_err("expected a parse error");
    assert!(matches!(err, ParseError::RawValueTooDeep { .. }));
}

/// Nested maps recurse through the same function as nested lists, so
/// they're capped by the same counter.
#[test]
fn deeply_nested_raw_maps_are_capped_too() {
    let n = hl_parser::MAX_RAW_VALUE_DEPTH + 1;
    let source = format!(
        "service s {{\n  image \"x\"\n  raw {{ k: {}1{} }}\n}}\n",
        "{ a: ".repeat(n),
        " }".repeat(n)
    );
    let err = parse(&source).expect_err("expected a parse error");
    assert!(matches!(err, ParseError::RawValueTooDeep { .. }));
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
