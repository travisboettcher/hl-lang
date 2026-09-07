use hl_lexer::TokenKind;
use hl_parser::schema::MapSide;
use hl_parser::{
    ArrowMapHost, Command, DependsOnCondition, Entrypoint, Expected, HealthcheckTest, Literal,
    ParseError, TemplateDecl, TopDecl, UseDecl, parse,
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

/// #181: the AST stores what a string literal *means*, so a `\n` in
/// source is a newline by the time any later stage sees it — not the two
/// characters that were typed.
#[test]
fn string_literal_holds_the_decoded_value() {
    let program = parse_ok("service s {\n  container_name \"a\\nb\\\"c\\\\d\"\n}\n");
    let service = as_service(&program.decls[0]);
    let name = service.fields.container_name.as_ref().unwrap();
    assert_eq!(name.text(), "a\nb\"c\\d");
    assert!(matches!(name, Literal::Str(_, _)));
}

/// Decoding shortens the value, so the span has to keep describing the
/// source text rather than the decoded string — a diagnostic pointing at
/// this literal still has to land on the characters the user wrote.
#[test]
fn a_decoded_literal_span_still_covers_its_source_text() {
    let source = "service s {\n  container_name \"a\\nb\"\n}\n";
    let program = parse_ok(source);
    let service = as_service(&program.decls[0]);
    let span = service.fields.container_name.as_ref().unwrap().span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "\"a\\nb\"",
        "span {span:?}"
    );
}

/// A string used as a field name is decoded the same way a value is.
#[test]
fn string_key_is_decoded_too() {
    let program = parse_ok("service s {\n  raw {\n    \"a\\tb\": \"v\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries[0].key.text(), "a\tb");
}

/// An escape sequence the language doesn't have never reaches the AST:
/// tokenizing fails first, and `parse` reports it as the lex error it is.
#[test]
fn unknown_escape_is_reported_as_a_lex_error() {
    let err = parse("service s {\n  container_name \"a\\qb\"\n}\n").unwrap_err();
    match &err {
        ParseError::Lex(errors) => assert!(
            matches!(errors[0], hl_lexer::LexError::UnknownEscape { ch: 'q', .. }),
            "{errors:?}"
        ),
        other => panic!("expected a lex error, got {other:?}"),
    }
    assert_eq!(
        err.to_string(),
        "2:20: unknown escape sequence `\\q` — a string literal supports \
         `\\\"`, `\\\\`, `\\n`, `\\t`, and `\\r`"
    );
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

// --- expose / `as` sugar (#198) ---

#[test]
fn expose_primary_only() {
    let program = parse_ok("service s {\n  expose 8096\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
}

/// `host` is no longer a field of `expose` at all (#198) — routing
/// fields live on `router` exclusively — so the pre-#198 explicit
/// comma-separated spelling is simply an unknown-field situation now: the
/// `host:` key doesn't resolve against `EXPOSE`'s own (port-only) field
/// list, the comma is left for the enclosing body, and a bare comma is
/// never a valid statement start there.
#[test]
fn expose_host_field_no_longer_parses() {
    let err = parse("service s {\n  expose 8096, host: \"host.example.com\"\n}\n").unwrap_err();
    assert!(
        matches!(err, ParseError::UnexpectedToken { .. }),
        "got {err:?}"
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

// --- command (#156) ---
//
// A plain scalar-or-list field directly on `service`/`template`, not a
// nested struct type — see `ast::ServiceFields::command`'s doc. Shares
// its grammar with `healthcheck.test` (#153) — a bare literal
// (Compose's shell form) or a bracketed list (Compose's exec form) — so
// these tests mirror that field's own tests above, minus the
// braced-body plumbing `command` doesn't need.

/// The shell form: a bare string with no braces, exactly like
/// `container_name`'s own bare-value shorthand.
#[test]
fn command_shell_form() {
    let program = parse_ok("service s {\n  command \"npm start\"\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.command.as_ref().unwrap() {
        Command::Shell(lit) => assert_eq!(lit.text(), "npm start"),
        other => panic!("expected Command::Shell, got {other:?}"),
    }
}

/// The explicit `key: value` spelling of the shell form also parses,
/// mirroring `container_name: "..."`.
#[test]
fn command_shell_form_with_colon() {
    let program = parse_ok("service s {\n  command: \"npm start\"\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.command.as_ref().unwrap() {
        Command::Shell(lit) => assert_eq!(lit.text(), "npm start"),
        other => panic!("expected Command::Shell, got {other:?}"),
    }
}

/// The exec form: a bracketed list of strings, matching the issue's own
/// `cadvisor.hll` example (#156).
#[test]
fn command_exec_form() {
    let program = parse_ok(
        "service s {\n  \
           command [\"--housekeeping_interval=30s\", \"--docker_only=true\"]\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    match service.fields.command.as_ref().unwrap() {
        Command::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(
                texts,
                vec!["--housekeeping_interval=30s", "--docker_only=true"]
            );
        }
        other => panic!("expected Command::Exec, got {other:?}"),
    }
}

/// A comma embedded inside one quoted list item is data, not a list
/// separator — `--enable_metrics=cpu,memory,network` has to survive as
/// one item, not split into three. This is the exact value the issue
/// calls out by name (#156).
#[test]
fn command_exec_form_item_with_embedded_comma_round_trips() {
    let program = parse_ok(
        "service s {\n  \
           command [\"--enable_metrics=cpu,memory,network\"]\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    match service.fields.command.as_ref().unwrap() {
        Command::Exec(items, _) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "--enable_metrics=cpu,memory,network");
        }
        other => panic!("expected Command::Exec, got {other:?}"),
    }
}

/// An exec-form list of exactly one item is still the list form, not
/// the shell form — brackets alone select exec syntax, matching
/// `healthcheck.test`'s own `healthcheck_test_list_form_single_item`.
#[test]
fn command_exec_form_single_item() {
    let program = parse_ok("service s {\n  command [\"npm\"]\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.command.as_ref().unwrap() {
        Command::Exec(items, _) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "npm");
        }
        other => panic!("expected Command::Exec, got {other:?}"),
    }
}

/// No `command` field at all leaves it unset — never defaulted or
/// inferred from the image.
#[test]
fn command_unset_by_default() {
    let program = parse_ok("service s {\n  image \"nginx\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.command.is_none());
}

/// Writing `command` twice is a duplicate-scalar compile error, same as
/// `healthcheck.test`'s own `healthcheck_duplicate_test_is_error` — a
/// single-occurrence field, not repeatable.
#[test]
fn command_duplicate_is_error() {
    let err = parse("service s {\n  command \"a\"\n  command \"b\"\n}\n").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateField {
                type_name: "service",
                field: "command",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// Deliberately no bare comma-list sugar, matching `healthcheck.test`
/// (see `schema::FieldKind::ScalarOrList`'s doc for why): a second
/// quoted string right after the first isn't a two-item list, so this
/// doesn't parse as `command ["a", "b"]` in disguise.
#[test]
fn command_bare_comma_list_is_rejected() {
    assert!(parse("service s {\n  command \"a\", \"b\"\n}\n").is_err());
}

// --- entrypoint (#183) ---
//
// Compose's `entrypoint:` key, overriding the image's `ENTRYPOINT`
// where `command` above overrides its `CMD`. Same
// `FieldKind::ScalarOrList` grammar as `command`, so these tests mirror
// that field's own directly. The identifier is shared with `expose`'s
// unrelated `entrypoint` sub-field, so the last two tests here pin down
// that the two roles stay apart.

/// The shell form: a bare string, exactly as the issue writes it.
#[test]
fn entrypoint_shell_form() {
    let program = parse_ok("service s {\n  entrypoint \"/bin/sh -c 'do-a-thing'\"\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.entrypoint.as_ref().unwrap() {
        Entrypoint::Shell(lit) => assert_eq!(lit.text(), "/bin/sh -c 'do-a-thing'"),
        other => panic!("expected Entrypoint::Shell, got {other:?}"),
    }
}

/// The explicit `key: value` spelling of the shell form also parses,
/// mirroring `command: "..."`.
#[test]
fn entrypoint_shell_form_with_colon() {
    let program = parse_ok("service s {\n  entrypoint: \"/entrypoint.sh\"\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.entrypoint.as_ref().unwrap() {
        Entrypoint::Shell(lit) => assert_eq!(lit.text(), "/entrypoint.sh"),
        other => panic!("expected Entrypoint::Shell, got {other:?}"),
    }
}

/// The exec form: a bracketed list of strings, the issue's second
/// spelling (#183).
#[test]
fn entrypoint_exec_form() {
    let program = parse_ok(
        "service s {\n  \
           entrypoint [\"/bin/sh\", \"-c\", \"do-a-thing\"]\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    match service.fields.entrypoint.as_ref().unwrap() {
        Entrypoint::Exec(items, _) => {
            let texts: Vec<&str> = items.iter().map(Literal::text).collect();
            assert_eq!(texts, vec!["/bin/sh", "-c", "do-a-thing"]);
        }
        other => panic!("expected Entrypoint::Exec, got {other:?}"),
    }
}

/// A comma inside one quoted item is data, not a list separator — the
/// same rule `command`'s own exec form follows.
#[test]
fn entrypoint_exec_form_item_with_embedded_comma_round_trips() {
    let program = parse_ok("service s {\n  entrypoint [\"/bin/sh -c a,b\"]\n}\n");
    let service = as_service(&program.decls[0]);
    match service.fields.entrypoint.as_ref().unwrap() {
        Entrypoint::Exec(items, _) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "/bin/sh -c a,b");
        }
        other => panic!("expected Entrypoint::Exec, got {other:?}"),
    }
}

/// No `entrypoint` field at all leaves it unset — never defaulted or
/// inferred from the image.
#[test]
fn entrypoint_unset_by_default() {
    let program = parse_ok("service s {\n  image \"nginx\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.entrypoint.is_none());
}

/// Writing `entrypoint` twice in one service body is a duplicate-scalar
/// compile error, same as `command`.
#[test]
fn entrypoint_duplicate_is_error() {
    let err = parse("service s {\n  entrypoint \"a\"\n  entrypoint \"b\"\n}\n").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateField {
                type_name: "service",
                field: "entrypoint",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// Deliberately no bare comma-list sugar, matching `command` — and
/// worth pinning separately here, since `router`'s own `entrypoints`
/// *does* take exactly that sugar. Two neighboring spellings, two
/// different grammars.
#[test]
fn entrypoint_bare_comma_list_is_rejected() {
    assert!(parse("service s {\n  entrypoint \"a\", \"b\"\n}\n").is_err());
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
        ArrowMapHost::Named(r) if r.text() == "media" && r.qualifier().is_none()
    ));
    assert!(matches!(&entries[1].host, ArrowMapHost::BindMount(lit) if lit.text() == "media"));
    assert!(matches!(&entries[2].host, ArrowMapHost::BindMount(lit) if lit.text() == "/mnt/x"));
    // `ArrowMapHost`'s own accessors read through either arm, and each
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
    let ArrowMapHost::Named(r) = &entries[0].host else {
        panic!("expected a named-volume host, got {:?}", entries[0].host);
    };
    assert_eq!(r.qualifier().unwrap().name, "common");
    assert_eq!(r.text(), "media");
}

/// The canonical map-body form takes both host kinds too, since it goes
/// through the same entry parser as the bare-entry sugar.
#[test]
fn volume_map_body_takes_both_host_kinds() {
    let program =
        parse_ok("service s {\n  volume {\n    media: \"/data\"\n    \"/mnt/x\": \"/x\"\n  }\n}\n");
    let entries = &as_service(&program.decls[0]).fields.volumes.entries;
    assert!(matches!(&entries[0].host, ArrowMapHost::Named(r) if r.text() == "media"));
    assert!(matches!(&entries[1].host, ArrowMapHost::BindMount(_)));
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

/// The same lookahead must not over-trigger: a comma followed by a
/// plain reference (no colon after it) still continues the list.
#[test]
fn bare_dns_list_still_continues_past_a_comma() {
    let program = parse_ok("service s {\n  dns a, b\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service.fields.dns.iter().map(|r| r.text()).collect();
    assert_eq!(names, vec!["a", "b"]);
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

// --- traefik (#159) ---

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

/// `labels` (#243) parses like `env` — a map-kind body with key-side
/// uniqueness — except its separator is `:`, so the canonical and
/// bare-entry forms are the same thing, as they are for `raw`.
///
/// The keys are the point of the test: Traefik's dotted, bracketed label
/// keys are the field's primary use, and they only reach the AST intact
/// if a quoted string key survives the lexer's own `.`-handling
/// untouched.
#[test]
fn labels_body_keeps_dotted_and_bracketed_keys_intact() {
    let program = parse_ok(
        "service s {\n  labels {\n               \"traefik.http.routers.s.tls.domains[0].main\": \"internal.example.com\"\n               \"com.example.owner\": \"platform-team\"\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let entries: Vec<(String, String)> = service
        .fields
        .labels
        .entries
        .iter()
        .map(|e| (e.key.text().to_string(), e.value.text()))
        .collect();
    assert_eq!(
        entries,
        vec![
            (
                "traefik.http.routers.s.tls.domains[0].main".to_string(),
                "internal.example.com".to_string()
            ),
            ("com.example.owner".to_string(), "platform-team".to_string()),
        ]
    );
}

/// The bare-entry form, with no braces — `labels`' separator is `:`, so
/// one entry written straight onto the field parses the same way one
/// inside a body does.
#[test]
fn labels_bare_entry_form() {
    let program = parse_ok("service s {\n  labels \"com.example.owner\": \"platform-team\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.labels.entries.len(), 1);
    assert_eq!(
        service.fields.labels.entries[0].key.text(),
        "com.example.owner"
    );
}

#[test]
fn labels_repeated_blocks_accumulate() {
    let program =
        parse_ok("service s {\n  labels { \"a\": \"1\" }\n  labels { \"b\": \"2\" }\n}\n");
    let service = as_service(&program.decls[0]);
    let keys: Vec<&str> = service
        .fields
        .labels
        .entries
        .iter()
        .map(|e| e.key.text())
        .collect();
    assert_eq!(keys, vec!["a", "b"]);
}

/// The whole reason the field is map-shaped rather than a list of
/// `"key=value"` strings: a repeated key is caught here, by the same
/// schema-declared uniqueness `env` uses, naming both spans.
#[test]
fn labels_duplicate_key_is_error() {
    let err = parse("service s {\n  labels { \"a\": \"1\", \"a\": \"2\" }\n}\n").unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "labels",
            side: MapSide::Key,
            value,
            ..
        } => {
            assert_eq!(value, "a");
        }
        other => panic!("expected DuplicateMapKey on a labels key, got {other:?}"),
    }
}

/// ...and across two blocks in one body, since they accumulate into one
/// map, exactly as two `env` statements or two `raw { }` blocks do.
#[test]
fn labels_duplicate_key_across_two_blocks_is_error() {
    let err = parse("service s {\n  labels { \"a\": \"1\" }\n  labels { \"a\": \"2\" }\n}\n")
        .unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateMapKey {
                type_name: "labels",
                ..
            }
        ),
        "expected DuplicateMapKey across two labels blocks"
    );
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

// --- maps: volume's `{ read_only }` flag (#158) ---

/// The plain bind-mount case the issue itself is about: a host path
/// mounted read-only with no top-level `volume` declaration involved at
/// all.
#[test]
fn volume_bind_mount_with_read_only_flag() {
    let program = parse_ok("service s {\n  volume \"/\" -> \"/rootfs\" { read_only }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert!(service.fields.volumes.entries[0].read_only);
}

/// And the named-volume case — the flag must work identically whether
/// the host side is [`ArrowMapHost::BindMount`] or [`ArrowMapHost::Named`],
/// since Compose's own `read_only` mount option applies to both alike.
#[test]
fn volume_named_volume_with_read_only_flag() {
    let program = parse_ok(
        "volume media {}\n\
         service s {\n  volume media -> \"/data\" { read_only }\n}\n",
    );
    let service = as_service(&program.decls[1]);
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert!(matches!(
        &service.fields.volumes.entries[0].host,
        ArrowMapHost::Named(r) if r.text() == "media"
    ));
    assert!(service.fields.volumes.entries[0].read_only);
}

/// No `{ read_only }` body at all is still legal, and must leave the flag
/// unset — the overwhelmingly common case, and the one whose emitted
/// Compose output must stay byte-for-byte unchanged (see
/// `hl-codegen`'s golden tests).
#[test]
fn volume_entry_without_body_leaves_read_only_unset() {
    let program = parse_ok("service s {\n  volume \"/mnt/media\" -> \"/data\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(!service.fields.volumes.entries[0].read_only);
}

/// The flag works the same way inside `volume`'s canonical multi-entry
/// body, entry by entry — one flagged, one not, in the same body — which
/// is also the shape that rules out the trailing-comma sugar the issue
/// itself first suggested: see [`hl_parser::ArrowMapEntry`]'s doc for why.
#[test]
fn volume_read_only_flag_in_canonical_multi_entry_body() {
    let program = parse_ok(
        "service s {\n  \
           volume {\n    \
             \"/\" -> \"/rootfs\" { read_only },\n    \
             \"/data\" -> \"/data\"\n  \
           }\n\
         }\n",
    );
    let service = as_service(&program.decls[0]);
    let entries = &service.fields.volumes.entries;
    assert_eq!(entries.len(), 2);
    assert!(entries[0].read_only, "first entry should be read-only");
    assert!(!entries[1].read_only, "second entry should not be flagged");
}

/// The bare-presence flag, exactly like `external`/`disable`, takes no
/// `:`/value — `{ read_only: true }` isn't legal syntax, it's an unknown
/// field, since `read_only` isn't a struct field resolved through the
/// generic engine here.
#[test]
fn volume_read_only_flag_rejects_a_colon_value() {
    let err =
        parse("service s {\n  volume \"/\" -> \"/rootfs\" { read_only: true }\n}\n").unwrap_err();
    assert!(
        matches!(err, ParseError::UnexpectedToken { .. }),
        "expected a parse error on the unexpected `:`, got {err:?}"
    );
}

/// Any other identifier inside a volume entry's `{ }` body is an unknown
/// field, matching `depends_on`'s own `{ condition: ... }` precedent
/// (#155) rather than silently accepting arbitrary Compose mount options
/// this milestone deliberately doesn't cover (`:z`, `:Z`, tmpfs sizing).
#[test]
fn volume_entry_body_rejects_an_unknown_flag() {
    let err =
        parse("service s {\n  volume \"/\" -> \"/rootfs\" { mode: \"ro\" }\n}\n").unwrap_err();
    match err {
        ParseError::UnknownField {
            type_name: "volume",
            field,
            raw_escape_hatch: false,
            ..
        } => assert_eq!(field, "mode"),
        other => panic!("expected UnknownField on volume entry body, got {other:?}"),
    }
}

/// The read-only flag rides along with whichever side collides — setting
/// it doesn't change what counts as a duplicate container path, since
/// uniqueness is still checked before the flag is even looked at.
#[test]
fn volume_duplicate_container_path_is_still_an_error_with_read_only_flag() {
    let err = parse(
        "service s {\n  \
           volume \"a\" -> \"/data\" { read_only }\n  \
           volume \"b\" -> \"/data\"\n\
         }\n",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateMapKey {
                type_name: "volume",
                side: MapSide::Value,
                ..
            }
        ),
        "expected DuplicateMapKey on volume container path, got {err:?}"
    );
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

/// #206: `raw` was the last map field where a key repeated inside one
/// body silently dropped a value — the same shape on `env` has always
/// named both spans. Both spans are asserted, not just the variant: what
/// makes this diagnostic worth anything is that it points at the second
/// occurrence *and* back at the first.
#[test]
fn raw_duplicate_key_in_one_body_is_error() {
    let err = parse("service s {\n  raw { user: \"1000\", user: \"2000\" }\n}\n").unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "raw",
            side: MapSide::Key,
            value,
            first,
            second,
        } => {
            assert_eq!(value, "user");
            assert_eq!((first.line, first.col), (2, 9));
            assert_eq!((second.line, second.col), (2, 23));
        }
        other => panic!("expected DuplicateMapKey on the raw key, got {other:?}"),
    }
}

/// Two `raw { }` blocks in one body accumulate into one map, so they
/// collide the same way two `env` statements already do
/// (`env_duplicate_key_is_error`) — the body is what scopes the check,
/// not the block. Before #206 this parsed, and the generated document
/// kept only the second value.
#[test]
fn raw_duplicate_key_across_two_blocks_in_one_body_is_error() {
    let err = parse("service s {\n  raw {\n    key: \"a\"\n  }\n  raw {\n    key: \"b\"\n  }\n}\n")
        .unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "raw",
            side: MapSide::Key,
            value,
            ..
        } => assert_eq!(value, "key"),
        other => panic!("expected DuplicateMapKey across two raw blocks, got {other:?}"),
    }
}

/// Two `raw` blocks that don't collide still accumulate, which is what
/// #206's check must not disturb.
#[test]
fn raw_distinct_keys_across_two_blocks_still_accumulate() {
    let program =
        parse_ok("service s {\n  raw {\n    a: \"1\"\n  }\n  raw {\n    b: \"2\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 2);
}

/// #206's "worth deciding first": duplicate-key-ness is a property of one
/// mapping, exactly as in YAML. Two sibling nested maps may each hold an
/// `x` — they're two mappings, not one — so this compiles.
#[test]
fn raw_sibling_nested_maps_may_each_repeat_a_key() {
    let program = parse_ok("service s {\n  raw { a: { x: 1 }, b: { x: 2 } }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 2);
}

/// The other half of the same rule: a nested map may reuse a key its
/// *enclosing* mapping already claims, since the two keys land in
/// different YAML mappings.
#[test]
fn raw_nested_map_may_reuse_an_enclosing_key() {
    let program = parse_ok("service s {\n  raw { x: 1, a: { x: 2 } }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.raw.entries.len(), 2);
}

/// And each nested map is checked on its own: a key repeated *within* one
/// of them is the same error the top level raises.
#[test]
fn raw_duplicate_key_inside_one_nested_map_is_error() {
    let err = parse("service s {\n  raw { opts: { a: 1, a: 2 } }\n}\n").unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "raw",
            side: MapSide::Key,
            value,
            first,
            second,
        } => {
            assert_eq!(value, "a");
            assert_eq!((first.line, first.col), (2, 17));
            assert_eq!((second.line, second.col), (2, 23));
        }
        other => panic!("expected DuplicateMapKey inside the nested map, got {other:?}"),
    }
}

/// A `raw` key is a plain literal, so the quoted and bare spellings of
/// one name the same key — and collide.
#[test]
fn raw_quoted_and_bare_spellings_of_one_key_collide() {
    let err = parse("service s {\n  raw { user: \"a\", \"user\": \"b\" }\n}\n").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::DuplicateMapKey {
                type_name: "raw",
                ref value,
                ..
            } if value == "user"
        ),
        "expected DuplicateMapKey across the two spellings, got {err:?}"
    );
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
fn networks_repeats_accumulate() {
    let program = parse_ok("service s {\n  networks [a]\n  networks [b]\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service.fields.networks.iter().map(|r| r.text()).collect();
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
        .map(|e| e.reference.text())
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

/// The bare comma-list sugar also works for `depends_on`, parsing every
/// comma-separated reference as its own entry rather than stopping
/// after the first — mirrors `networks_comma_sugar_form` for the
/// analogous reference-list sugar. Exercises
/// `parse_bare_depends_on_list`'s own comma-continuation loop directly:
/// each entry here (`db`, then `cache`) is followed by another bare
/// `IDENT`, never a `KEY :` pair, so `comma_starts_a_new_field` must
/// correctly say "no, that's not a new field" for the loop to keep
/// consuming instead of stopping after just `db`.
#[test]
fn depends_on_bare_comma_list_form() {
    let program = parse_ok("service s {\n  depends_on db, cache\n}\n");
    let service = as_service(&program.decls[0]);
    let names: Vec<&str> = service
        .fields
        .depends_on
        .iter()
        .map(|e| e.reference.text())
        .collect();
    assert_eq!(names, vec!["db", "cache"]);
}

/// `db { condition: service_healthy }` (#155): the bracketed extended
/// form carries its condition alongside the plain reference.
#[test]
fn depends_on_extended_form_parses_the_condition() {
    let program = parse_ok("service s {\n  depends_on [db { condition: service_healthy }]\n}\n");
    let service = as_service(&program.decls[0]);
    let entry = &service.fields.depends_on[0];
    assert_eq!(entry.reference.text(), "db");
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
    assert_eq!(service.fields.depends_on[0].reference.text(), "cache");
    assert!(service.fields.depends_on[0].condition.is_none());
    assert_eq!(service.fields.depends_on[1].reference.text(), "db");
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

/// `DependsOnCondition::compose_value` — the string
/// [`hl_codegen::generate_depends_on`]'s long map form actually writes
/// into the `condition:` key — round-trips through [`DependsOnCondition::parse`]
/// to exactly the same spelling it was parsed from, for each of
/// Compose's own three values. Checked directly against the enum here
/// rather than only through generated YAML, since a golden-test
/// assertion in another crate isn't exercised while this crate's own
/// mutation-testing run is scoped to `hl-parser`.
#[test]
fn depends_on_condition_compose_value_matches_its_own_spelling() {
    for (condition, expected) in [
        (DependsOnCondition::ServiceStarted, "service_started"),
        (DependsOnCondition::ServiceHealthy, "service_healthy"),
        (
            DependsOnCondition::ServiceCompletedSuccessfully,
            "service_completed_successfully",
        ),
    ] {
        assert_eq!(condition.compose_value(), expected);
        assert_eq!(
            DependsOnCondition::parse(expected),
            Some(condition),
            "{expected:?} did not parse back to the condition its own compose_value produced"
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
    let names: Vec<&str> = service.fields.networks.iter().map(|r| r.text()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn dns_bracket_list_form() {
    let program = parse_ok("service s {\n  dns [\"192.168.50.182\"]\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["192.168.50.182"]);
}

#[test]
fn dns_repeats_accumulate() {
    let program = parse_ok("service s {\n  dns \"192.168.50.182\"\n  dns \"192.168.50.183\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.dns.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["192.168.50.182", "192.168.50.183"]);
}

/// `env_file "one.env"` — the bare single-item sugar every reference-list
/// field gets for free (#154).
#[test]
fn env_file_bare_single_form() {
    let program = parse_ok("service s {\n  env_file \"miniflux.env\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.env_file.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["miniflux.env"]);
}

#[test]
fn env_file_bracket_list_form() {
    let program = parse_ok("service s {\n  env_file [\"miniflux.env\", \"common.env\"]\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.env_file.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["miniflux.env", "common.env"]);
}

#[test]
fn env_file_repeats_accumulate() {
    let program =
        parse_ok("service s {\n  env_file \"miniflux.env\"\n  env_file \"common.env\"\n}\n");
    let service = as_service(&program.decls[0]);
    let entries: Vec<&str> = service.fields.env_file.iter().map(|r| r.text()).collect();
    assert_eq!(entries, vec!["miniflux.env", "common.env"]);
}

// --- privileged / devices (#157) ---

/// `privileged` is bare-presence only, modeled directly on `network`'s
/// `external` — see `bool_flag_rejects_explicit_value`/
/// `bool_flag_duplicate_is_error` for the generic mechanism this
/// exercises.
#[test]
fn privileged_bare_flag_on_service() {
    let program = parse_ok("service s {\n  image \"x\"\n  privileged\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.privileged.is_some());
}

#[test]
fn service_without_privileged_defaults_unset() {
    let program = parse_ok("service s {\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.privileged.is_none());
}

#[test]
fn privileged_rejects_a_colon_value() {
    let err = parse("service s {\n  image \"x\"\n  privileged: true\n}\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

/// `devices` is map-kind since #167 (review feedback on #157's original
/// pre-joined `"host:container"` string), spelled with `publish`'s own
/// `->` bare-entry sugar — see `publish_arrow_sugar_bare_entry` for the
/// mirrored test.
#[test]
fn devices_arrow_sugar_bare_entry() {
    let program = parse_ok("service s {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.devices.entries.len(), 1);
    assert_eq!(service.fields.devices.entries[0].host.text(), "/dev/kmsg");
    assert_eq!(
        service.fields.devices.entries[0].container.text(),
        "/dev/kmsg"
    );
}

#[test]
fn devices_colon_canonical_body() {
    let program = parse_ok(
        "service s {\n  devices { \"/dev/kmsg\": \"/dev/kmsg\", \"/dev/fuse\": \"/dev/fuse\" }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.devices.entries.len(), 2);
    assert_eq!(service.fields.devices.entries[0].host.text(), "/dev/kmsg");
    assert_eq!(
        service.fields.devices.entries[1].container.text(),
        "/dev/fuse"
    );
}

/// Repeating the field accumulates rather than being a duplicate-scalar
/// error, exactly like `publish_repeats_accumulate`.
#[test]
fn devices_repeats_accumulate() {
    let program = parse_ok(
        "service s {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n  devices \"/dev/fuse\" -> \"/dev/fuse\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
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

/// A quoted container side carries Compose's optional cgroup
/// permissions suffix (`HOST:CONTAINER[:CGROUP_PERMISSIONS]`), the
/// direct analogue of `publish`'s protocol suffix — see
/// `publish_accepts_a_quoted_protocol_suffix_on_the_container_side`.
#[test]
fn devices_accepts_a_permissions_suffix_on_the_container_side() {
    let program = parse_ok("service s {\n  devices \"/dev/sda\" -> \"/dev/xvda:rwm\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.devices.entries.len(), 1);
    assert_eq!(service.fields.devices.entries[0].host.text(), "/dev/sda");
    assert_eq!(
        service.fields.devices.entries[0].container.text(),
        "/dev/xvda:rwm"
    );
}

/// Uniqueness is checked on the container side, matching `publish`'s own
/// convention (see `schema::DEVICES`'s doc for why): two entries mapping
/// different hosts onto the same container path collide.
#[test]
fn devices_duplicate_container_path_is_error() {
    let err = parse(
        "service s {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n  devices \"/dev/fuse\" -> \"/dev/kmsg\"\n}\n",
    )
    .unwrap_err();
    match err {
        ParseError::DuplicateMapKey {
            type_name: "devices",
            side: MapSide::Value,
            value,
            ..
        } => assert_eq!(value, "/dev/kmsg"),
        other => panic!("expected DuplicateMapKey on devices container path, got {other:?}"),
    }
}

/// The same host device mapped onto two different container paths is
/// legitimate — a host-side check would have rejected it, which is why
/// uniqueness lands on the container side instead.
#[test]
fn devices_same_host_different_container_paths_is_accepted() {
    let program = parse_ok(
        "service s {\n  devices \"/dev/sda\" -> \"/dev/xvda:r\"\n  devices \"/dev/sda\" -> \"/dev/xvdb:rwm\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(service.fields.devices.entries.len(), 2);
}

#[test]
fn devices_entry_missing_separator_is_an_error() {
    let err = parse("service s {\n  devices \"/dev/kmsg\"\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::MapEntryMissingSeparator {
            type_name: "devices",
            separator: TokenKind::Arrow,
            ..
        }
    ));
}

/// A `template` body accepts exactly the same fields as a `service` one.
#[test]
fn devices_is_accepted_in_a_template_body() {
    let program = parse_ok("template t {\n  devices \"/dev/kmsg\" -> \"/dev/kmsg\"\n}\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(template.fields.devices.entries.len(), 1);
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
}

#[test]
fn template_decl_without_a_body_is_error() {
    // `template t = <statement>` parsed until #194 removed it, so this
    // pins the removal rather than the general "no body" case: nothing
    // else fails if the `=` arm comes back.
    let err = parse("template t = restart unless-stopped\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));

    let err = parse("template t\n").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

/// #201 dropped the `: Number`/`: String` annotation the grammar used to
/// allow here — a parameter is just a bare name now, and a `:` right
/// after one is a parse error rather than the start of a type.
#[test]
fn param_list_type_annotation_is_parse_error() {
    let err = parse("template t(a: Number) {\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::RParen),
            found_kind: TokenKind::Colon,
            ..
        }
    ));
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
    assert_eq!(r.qualifier().unwrap().name, "traefik");
    assert_eq!(r.text(), "traefik-net");
}

#[test]
fn unqualified_reference_has_no_qualifier() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks [traefik-net]\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert!(r.qualifier().is_none());
    assert_eq!(r.text(), "traefik-net");
}

#[test]
fn qualified_reference_bare_comma_form() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks common.traefik-net\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert_eq!(r.qualifier().unwrap().name, "common");
    assert_eq!(r.text(), "traefik-net");
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

// --- field access in a value position (#275) ---
//
// `.` parses two ways now, and which one a position gets is decided by
// the position, not by the tokens: a *value* reads `a.b` as a field
// access, a *reference* reads it as `alias.name`. These pin both halves,
// since the whole design rests on the two never competing.

/// The value the field access resolves to is composition's business —
/// what the parser has to get right is the shape.
fn field_access(lit: &Literal) -> &hl_parser::FieldAccess {
    match lit {
        Literal::Field(access) => access,
        other => panic!("expected a field access, got {other:?}"),
    }
}

/// The literal a service's first `labels` entry holds, for the cases
/// that care about its *kind* rather than its text. A list-valued entry
/// (#288) has no single literal, so those cases would be asking the
/// wrong question — they all write a scalar.
fn first_label_value(program: &hl_parser::Program) -> &Literal {
    scalar_label_value(&as_service(&program.decls[0]).fields.labels.entries[0].value)
}

/// The one literal a scalar-valued `labels` entry holds. A list-valued
/// entry (#288) has none, and every case reaching for this writes a
/// single value — asking for the literal is the question.
fn scalar_label_value(value: &hl_parser::LabelValue) -> &Literal {
    match value {
        hl_parser::LabelValue::Scalar(lit) => lit,
        hl_parser::LabelValue::List(_, _) => {
            panic!("this case writes a single label value, not a list")
        }
    }
}

#[test]
fn two_segment_field_access_names_a_local_declaration() {
    let program =
        parse_ok("service s {\n  image \"x\"\n  labels { \"caddy.network\": proxy.name }\n}\n");
    let access = field_access(first_label_value(&program));
    assert_eq!(
        access.base,
        Literal::Ident("proxy".to_string(), access.base.span())
    );
    assert_eq!(access.field.name, "name");
    assert_eq!(access.dotted(), "proxy.name");
}

#[test]
fn three_segment_field_access_names_an_imported_declaration() {
    let source =
        "service s {\n  image \"x\"\n  labels { \"caddy.network\": traefik.proxy.name }\n}\n";
    let program = parse_ok(source);
    let access = field_access(first_label_value(&program));
    assert_eq!(access.base.qualifier().unwrap().name, "traefik");
    assert_eq!(access.base.text(), "proxy");
    assert_eq!(access.field.name, "name");
    assert_eq!(access.dotted(), "traefik.proxy.name");
    // The base is the alias-and-declaration half, and its span says so:
    // it stops before the field, so a diagnostic about resolving the
    // declaration underlines only the part that names one.
    let base = access.base.span();
    assert_eq!(
        &source[base.start as usize..base.end as usize],
        "traefik.proxy"
    );
}

/// The access's span runs from its head through its last segment, so a
/// diagnostic about the value underlines what the author wrote rather
/// than the declaration half alone.
#[test]
fn a_field_access_span_covers_the_whole_access() {
    let source = "service s {\n  image \"x\"\n  labels { \"k\": traefik.proxy.name }\n}\n";
    let program = parse_ok(source);
    let span = first_label_value(&program).span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "traefik.proxy.name"
    );
}

#[test]
fn a_parameter_can_carry_a_field_access() {
    let program = parse_ok("template t(net) {\n  labels { \"caddy.network\": $net.name }\n}\n");
    let template = as_template(&program.decls[0]);
    let access = field_access(scalar_label_value(&template.fields.labels.entries[0].value));
    assert!(matches!(access.base, Literal::Param(ref name, _) if name == "net"));
    assert_eq!(access.dotted(), "$net.name");
}

/// A fourth segment has no shape left to be — see
/// `Parser::parse_field_access` — so it's refused rather than read as
/// the first three with the rest dropped.
#[test]
fn four_segment_field_access_is_too_deep() {
    let err = parse("service s {\n  image \"x\"\n  labels { \"k\": a.b.c.d }\n}\n")
        .expect_err("four dotted segments should not parse");
    match err {
        ParseError::FieldAccessTooDeep { text, .. } => assert_eq!(text, "a.b.c.d"),
        other => panic!("expected FieldAccessTooDeep, got {other:?}"),
    }
}

/// A parameter already names one declaration, so it has no alias
/// segment to spell — `$net.a.b` is one field too many, not three
/// segments' worth of something else.
#[test]
fn a_parameter_base_takes_exactly_one_field() {
    let err = parse("template t(net) {\n  labels { \"k\": $net.a.b }\n}\n")
        .expect_err("two fields after a parameter should not parse");
    match err {
        ParseError::FieldAccessTooDeep { text, .. } => assert_eq!(text, "$net.a.b"),
        other => panic!("expected FieldAccessTooDeep, got {other:?}"),
    }
}

#[test]
fn field_access_is_rejected_in_a_reference_position() {
    let err = parse("service s {\n  image \"x\"\n  networks [traefik.proxy.name]\n}\n")
        .expect_err("a field access should not parse as a network reference");
    match err {
        ParseError::FieldAccessInReferencePosition { text, .. } => {
            assert_eq!(text, "traefik.proxy.name");
        }
        other => panic!("expected FieldAccessInReferencePosition, got {other:?}"),
    }
}

/// The rejection consumes the trailing segments before reporting, so
/// its span covers the whole access rather than stopping at the
/// reference that parsed — the message quotes the same text the span
/// underlines.
#[test]
fn the_reference_position_rejection_spans_the_whole_access() {
    let source = "service s {\n  image \"x\"\n  networks [traefik.proxy.name]\n}\n";
    let err = parse(source).expect_err("a field access should not parse as a network reference");
    let span = err.span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "traefik.proxy.name"
    );
}

/// The `$param` spelling is the other one a reference position can tell
/// apart from `alias.name`, and it's refused for the same reason.
#[test]
fn a_parameter_field_access_is_rejected_in_a_reference_position() {
    let err = parse("template t(net) {\n  networks [$net.name]\n}\n")
        .expect_err("a field access should not parse as a network reference");
    match err {
        ParseError::FieldAccessInReferencePosition { text, .. } => {
            assert_eq!(text, "$net.name");
        }
        other => panic!("expected FieldAccessInReferencePosition, got {other:?}"),
    }
}

/// Every reference-shaped position rejects it, not just `networks` —
/// they all reach one parsing function. A named-volume mount's host
/// side is the other position that resolves a qualifier, so it's the
/// one worth pinning beside `networks`.
#[test]
fn field_access_is_rejected_in_a_volume_mount_host() {
    let err = parse("service s {\n  image \"x\"\n  volume storage.media.name -> \"/data\"\n}\n")
        .expect_err("a field access should not parse as a volume reference");
    assert!(matches!(
        err,
        ParseError::FieldAccessInReferencePosition { .. }
    ));
}

/// The two-segment spelling is what a reference position keeps: it's
/// indistinguishable from the `alias.name` it has always been, so
/// `networks [traefik.proxy]` goes on meaning the imported network.
#[test]
fn a_two_segment_reference_is_still_a_qualified_reference() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks [traefik.proxy]\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.networks[0];
    assert_eq!(r.qualifier().unwrap().name, "traefik");
    assert_eq!(r.text(), "proxy");
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
fn qualified_depends_on_reference_parses() {
    // Parsing accepts a qualified reference on any reference-shaped
    // field, `depends_on` included — compose() is what rejects it as
    // unsupported (schema-agnostic parser, per the codebase's existing
    // "don't special-case field identity in the parser" precedent).
    let program = parse_ok("service s {\n  image \"x\"\n  depends_on [other.db]\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &service.fields.depends_on[0].reference;
    assert_eq!(r.qualifier().unwrap().name, "other");
    assert_eq!(r.text(), "db");
}

// --- routing's migration diagnostics (#271) ---

/// The three names #271 removed each keep being *recognized*, purely so
/// the message can say where routing went. `UnknownField` would answer
/// them with its `raw { ... }` hint instead, which is actively wrong
/// advice here: `raw` replaces the whole label list rather than adding
/// to it, so following it would compile and then silently drop every
/// other label the service has.
#[test]
fn a_router_block_names_its_new_home() {
    let err = parse("service s {\n  image \"x\"\n  router { host: \"a\" }\n}\n")
        .expect_err("`router` should no longer resolve");
    let ParseError::MovedField { field, .. } = &err else {
        panic!("expected MovedField, got {err:?}");
    };
    assert_eq!(*field, "router");
    let rendered = err.to_string();
    assert!(rendered.contains("std:traefik"), "{rendered}");
    assert!(rendered.contains("traefik.http"), "{rendered}");
}

/// The same on a `template` body, which is where a homelab's routing
/// actually lives — a file that never writes `router` on a `service`
/// still has it in every template it shares.
#[test]
fn a_router_block_in_a_template_names_its_new_home_too() {
    let err = parse("template t {\n  router { host: \"a\" }\n}\n")
        .expect_err("`router` should no longer resolve in a template");
    assert!(
        matches!(&err, ParseError::MovedField { field, .. } if *field == "router"),
        "{err:?}"
    );
}

/// `traefik { disable }` gets its own message rather than sharing
/// `router`'s: the replacement is a different template, and pointing a
/// reader at `traefik.http` when they wanted `traefik.disable` is a
/// wrong turn a diagnostic shouldn't cause.
#[test]
fn a_traefik_block_names_its_new_home() {
    let err = parse("service s {\n  image \"x\"\n  traefik {\n    disable\n  }\n}\n")
        .expect_err("`traefik` should no longer resolve");
    assert!(
        matches!(&err, ParseError::MovedField { field, .. } if *field == "traefik"),
        "{err:?}"
    );
    let rendered = err.to_string();
    assert!(rendered.contains("traefik.disable"), "{rendered}");
}

/// The service-level `middleware` spelling #221 moved onto `router` is
/// still recognized two moves later, and now names the label template
/// rather than the field that also went away.
#[test]
fn a_service_level_middleware_names_its_new_home() {
    let err = parse("service s {\n  image \"x\"\n  middleware auth\n}\n")
        .expect_err("`middleware` should no longer resolve");
    assert!(
        matches!(&err, ParseError::MovedField { field, .. } if *field == "middleware"),
        "{err:?}"
    );
    let rendered = err.to_string();
    assert!(rendered.contains("http_middlewares"), "{rendered}");
}

/// `expose <port> as "<host>"` is the one removed spelling that isn't a
/// field name, so it can't go through `moved_field` — the parser
/// recognizes the `as` itself and says the same kind of thing.
#[test]
fn the_expose_as_sugar_names_its_replacement() {
    let err = parse("service s {\n  expose 8096 as \"media.example.com\"\n}\n")
        .expect_err("`expose ... as` should no longer parse");
    let rendered = err.to_string();
    assert!(rendered.contains("was removed with routing"), "{rendered}");
    assert!(rendered.contains("expose <port>"), "{rendered}");
    assert!(rendered.contains("std:traefik"), "{rendered}");
}

/// A name that never existed still gets the ordinary `UnknownField`,
/// so the migration table stays a short list of real former spellings
/// rather than a catch-all that swallows typos.
#[test]
fn an_unrelated_unknown_field_is_not_a_moved_field() {
    let err = parse("service s {\n  image \"x\"\n  routerz { host: \"a\" }\n}\n")
        .expect_err("`routerz` is not a field");
    assert!(matches!(&err, ParseError::UnknownField { .. }), "{err:?}");
}

// --- comma-continued secondary fields ---

/// The generic continuation every struct-kind field with more than one
/// sub-field rides: after a primary value, `, key: value` keeps setting
/// fields of the *same* type rather than starting a sibling statement.
///
/// `router api, host: "..."` used to be this rule's main exercise, and
/// #271 removed it — so it is pinned here directly rather than through
/// whichever field happens to use it, since the rule outlives any one
/// of them.
#[test]
fn a_comma_continues_into_a_secondary_field() {
    let program = parse_ok(
        "service s {\n  image \"x\"\n  build \"./app\", dockerfile: \"Dockerfile.prod\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let build = service.fields.build.as_ref().expect("build set");
    assert_eq!(build.context.as_ref().unwrap().text(), "./app");
    assert_eq!(build.dockerfile.as_ref().unwrap().text(), "Dockerfile.prod");
}

/// The continuation is checked against the *enclosing type's* own field
/// list, so a comma followed by something that isn't one of its fields
/// ends the statement instead of being swallowed. Without that, the
/// stray text would be consumed as part of this field and the
/// diagnostic would land somewhere the author never wrote.
#[test]
fn a_comma_before_an_unknown_key_ends_the_statement() {
    let err = parse("service s {\n  image \"x\"\n  build \"./app\", nonsense: \"x\"\n}\n")
        .expect_err("`nonsense` is not a `build` field");
    let rendered = err.to_string();
    assert!(rendered.contains("found `,`"), "{rendered}");
}

/// A bare reference list ends at a comma whose next tokens are `KEY :`,
/// because that shape starts a field rather than another list item. The
/// span is the assertion: the error lands on the *comma*, not on the
/// `:` several tokens later, which is what tells a reader the list was
/// the thing that ended. Swallowing the key first would report against
/// text the list was never entitled to.
#[test]
fn a_bare_reference_list_stops_at_a_following_key() {
    let source = "service s {\n  networks a, b, image: \"x\"\n}\n";
    let err = parse(source).expect_err("a `key:` cannot continue a reference list");
    let rendered = err.to_string();
    assert!(rendered.contains("found `,`"), "{rendered}");
    let span = err.span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        ",",
        "the diagnostic should point at the comma that ended the list"
    );
}

/// The other half: a comma *not* followed by `KEY :` goes on continuing
/// the list, so the lookahead is a real discriminator rather than a
/// blanket stop.
#[test]
fn a_bare_reference_list_continues_past_an_ordinary_comma() {
    let program = parse_ok("service s {\n  image \"x\"\n  networks a, b, c\n}\n");
    let service = as_service(&program.decls[0]);
    let nets: Vec<&str> = service.fields.networks.iter().map(|r| r.text()).collect();
    assert_eq!(nets, vec!["a", "b", "c"]);
}

/// The service-level `entrypoint` is untouched by that rename — it's
/// Compose's own key, and it still parses in a `service` body. Pinned
/// beside the preceding test so a future edit to `moved_field` can't
/// quietly start refusing it.
#[test]
fn service_level_entrypoint_still_parses_after_the_router_rename() {
    let program = parse_ok("service s {\n  entrypoint \"/bin/sh\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.entrypoint.is_some());
}

// --- `build` (#224) ---

/// The bare-context form, Compose's own short spelling — `build`'s
/// primary field, exactly as `ref` is `image`'s.
#[test]
fn build_primary_shorthand_parses() {
    let program = parse_ok("service s {\n  build \"./vault-git-sync\"\n}\n");
    let service = as_service(&program.decls[0]);
    let build = service.fields.build.as_ref().expect("build set");
    assert_eq!(build.context.as_ref().unwrap().text(), "./vault-git-sync");
    assert!(build.dockerfile.is_none());
}

/// The braced form, for a build that names a `dockerfile` too.
#[test]
fn build_braced_body_parses() {
    let program = parse_ok(
        "service s {\n  build {\n    context: \"./app\"\n    \
         dockerfile: \"Dockerfile.prod\"\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let build = service.fields.build.as_ref().expect("build set");
    assert_eq!(build.context.as_ref().unwrap().text(), "./app");
    assert_eq!(build.dockerfile.as_ref().unwrap().text(), "Dockerfile.prod");
}

/// `build` and `image` are independent fields, both settable — Compose
/// takes the pair to mean "build this context, then tag it as that".
#[test]
fn build_and_image_can_both_be_set() {
    let program = parse_ok("service s {\n  image \"app:latest\"\n  build \"./app\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.image.is_some());
    assert!(service.fields.build.is_some());
}

/// A `$param` reaches both sub-fields, so a template can parameterize
/// which context it builds.
#[test]
fn build_accepts_params() {
    let program =
        parse_ok("template t(c, d) {\n  build {\n    context: $c\n    dockerfile: $d\n  }\n}\n");
    let template = as_template(&program.decls[0]);
    let build = template.fields.build.as_ref().expect("build set");
    assert!(matches!(build.context.as_ref().unwrap(), Literal::Param(n, _) if n == "c"));
    assert!(matches!(build.dockerfile.as_ref().unwrap(), Literal::Param(n, _) if n == "d"));
}

/// An unknown sub-field is refused against `build`'s own field list.
#[test]
fn unknown_build_field_is_rejected() {
    let err = parse("service s {\n  build { args: 1 }\n}\n").expect_err("expected an error");
    assert!(
        matches!(
            err,
            ParseError::UnknownField {
                type_name: "build",
                ref field,
                ..
            } if field == "args"
        ),
        "got {err:?}"
    );
}

/// `build` is struct-kind, so writing it twice in one body is the
/// ordinary duplicate-field error, not an accumulation.
#[test]
fn duplicate_build_is_rejected() {
    let err = parse("service s {\n  build \"./a\"\n  build \"./b\"\n}\n")
        .expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::DuplicateField {
                type_name: "service",
                field: "build",
                ..
            }
        ),
        "got {err:?}"
    );
}

// --- `priority`, `port`, `protocol` on a router (#225) ---
