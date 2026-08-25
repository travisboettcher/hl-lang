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
    assert!(service.fields.routers.is_empty());
}

/// `expose <port> as "<host>"` desugars to `expose { port }` plus an
/// unnamed `router { host }` (#198) — `host` no longer lives on `Expose`
/// itself, so the spelling survives as bespoke parser sugar reaching for
/// `router` instead. This is the hard constraint the whole issue rests
/// on: the sugar must keep parsing to *something* that emits the exact
/// labels it always did (see `hl-codegen`'s own
/// `sugared_expose_as_router_emits_exactly_what_expose_host_always_did`).
#[test]
fn expose_as_sugar_desugars_to_port_plus_unnamed_router() {
    let program = parse_ok("service s {\n  expose 8096 as \"host.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let expose = service.fields.expose.as_ref().unwrap();
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
    assert_eq!(service.fields.routers.len(), 1);
    let router = &service.fields.routers[0];
    assert_eq!(router.key(), None);
    assert_eq!(router.host.as_ref().unwrap().text(), "host.example.com");
    assert!(router.entrypoint.is_empty());
    assert!(router.path_prefix.is_empty());
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

/// `as` fuses onto the primary value as one self-contained unit (docs/
/// DESIGN.md's desugaring rule 3) — it cannot be followed by further
/// secondary fields, comma or no comma, exactly as the pre-#198 schema-
/// driven alias sugar it replaced. A service that needs more than a bare
/// host must write the router out explicitly (`expose <port>` plus
/// `router { host: "...", entrypoint: ... }`).
///
/// Unlike before #198, there's no dedicated diagnostic for this dead end
/// any more (`ParseError::AliasSugarCannotContinue` is gone — see F6 of
/// #198): whatever follows is left for the enclosing body's own
/// statement loop, which reports the generic "expected a field name"
/// error a bare comma there always produces.
#[test]
fn alias_sugar_cannot_be_followed_by_further_secondary_fields() {
    let err = parse(
        "service s {\n  expose 8096 as \"host.example.com\", entrypoint: \"web-secure\"\n}\n",
    )
    .unwrap_err();
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
/// worth pinning separately here, since `router`'s own `entrypoint`
/// *does* take exactly that sugar. The two fields share a name, not a
/// grammar.
#[test]
fn entrypoint_bare_comma_list_is_rejected() {
    assert!(parse("service s {\n  entrypoint \"a\", \"b\"\n}\n").is_err());
}

/// The service-level field and `router`'s reference-list sub-field
/// coexist in one body, each resolved against its own enclosing type's
/// field list: the bare `entrypoint` statement sets `ServiceFields`'s
/// scalar-or-list field, while the one inside `router`'s body sets
/// `Router::entrypoint` — an unrelated field two levels removed, not the
/// same slot under a different name.
#[test]
fn service_entrypoint_and_router_entrypoint_coexist() {
    let program = parse_ok(
        "service s {\n  \
           image \"nginx\"\n  \
           entrypoint [\"/bin/sh\", \"-c\", \"do-a-thing\"]\n  \
           expose 8080\n  \
           router {\n    host: \"s.example.com\"\n    entrypoint: web, web-secure\n  }\n\
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
    let router = &service.fields.routers[0];
    let names: Vec<&str> = router.entrypoint.iter().map(|r| r.text()).collect();
    assert_eq!(names, vec!["web", "web-secure"]);
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

fn router_entrypoints_of(router: &hl_parser::Router) -> Vec<&str> {
    router.entrypoint.iter().map(|r| r.text()).collect()
}

/// `entrypoint` is a reference list, spelled exactly like `networks`:
/// a bare comma-separated list, a bracketed list, a quoted name, or a
/// repeat of the field, all of which accumulate.
#[test]
fn router_entrypoint_accepts_a_bracketed_list_and_a_quoted_name() {
    let program = parse_ok(
        "service s {\n  router {\n    host: \"a.example.com\"\n    \
         entrypoint: [web, web-secure]\n    entrypoint: \"metrics\"\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let router = &service.fields.routers[0];
    assert_eq!(
        router_entrypoints_of(router),
        vec!["web", "web-secure", "metrics"]
    );
}

#[test]
fn router_entrypoint_accepts_a_bare_comma_list() {
    // The book documents this spelling for `router`'s reference list,
    // the same one `networks` takes. #198 moved `entrypoint` off
    // `expose`, and the deleted `expose_entrypoint_accepts_a_bare_list`
    // was the only test covering it — the replacement covers the
    // bracketed and quoted forms but not this one.
    let program = parse_ok(
        "service s {\n  router {\n    host: \"a.example.com\"\n    \
         entrypoint: web, web-secure\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let router = &service.fields.routers[0];
    assert_eq!(router_entrypoints_of(router), vec!["web", "web-secure"]);
}

#[test]
fn expose_as_sugar_router_span_covers_through_the_host() {
    // The desugared router's span runs from the `as` keyword through the
    // closing quote of the host, so a diagnostic about it points at the
    // whole sugar rather than at the keyword alone.
    let source = "service s {\n  expose 80 as \"h.example.com\"\n}\n";
    let program = parse_ok(source);
    let service = as_service(&program.decls[0]);
    let span = service.fields.routers[0].span;
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "as \"h.example.com\""
    );
}

/// `host`/`entrypoint` together on the unnamed router, via the braced
/// body — the shape `docs/DESIGN.md`'s `internal_web` template uses (the
/// unnamed form has no name to continue a comma-list from, so the
/// braced body is its only multi-field spelling). Exercised inside a
/// `template` body specifically, matching that real worked example.
#[test]
fn router_host_and_entrypoint_fields_in_template_body() {
    let program = parse_ok(
        "template internal_web(port) {\n  \
           expose $port\n  \
           router {\n    host: \"{{name}}.internal.techdebtor.io\"\n    entrypoint: \"web-secure\"\n  }\n  \
           dns \"192.168.50.182\"\n\
         }\n",
    );
    let template = as_template(&program.decls[0]);
    let router = &template.fields.routers[0];
    assert_eq!(
        router.host.as_ref().unwrap().text(),
        "{{name}}.internal.techdebtor.io"
    );
    assert_eq!(router_entrypoints_of(router), vec!["web-secure"]);
    assert_eq!(template.fields.dns.len(), 1);
}

/// A bare `entrypoint` list stops at the next `key:` rather than
/// swallowing it as another entry point — the one-token lookahead in
/// `parse_bare_reference_list`. Without it, `host` would be read as a
/// second entry point and the parse would then die on its `:` with an
/// error pointing at the wrong place entirely.
#[test]
fn bare_entrypoint_list_stops_at_the_next_field_key() {
    let program =
        parse_ok("service s {\n  router api, entrypoint: web, host: \"x.example.com\"\n}\n");
    let service = as_service(&program.decls[0]);
    let router = &service.fields.routers[0];
    assert_eq!(router_entrypoints_of(router), vec!["web"]);
    assert_eq!(router.host.as_ref().unwrap().text(), "x.example.com");
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

#[test]
fn traefik_disabled_bare_flag() {
    let program = parse_ok("service s {\n  image \"x\"\n  traefik {\n    disabled\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.traefik.as_ref().unwrap().disabled.is_some());
}

#[test]
fn service_without_traefik_field_defaults_to_none() {
    let program = parse_ok("service s {\n  image \"x\"\n}\n");
    let service = as_service(&program.decls[0]);
    assert!(service.fields.traefik.is_none());
}

/// `disabled` is bare-presence only, exactly like `network`'s `external`
/// and `healthcheck`'s `disable` — a `:` after it is rejected rather than
/// treated as an attempted value.
#[test]
fn traefik_disabled_rejects_a_colon_value() {
    let err = parse("service s {\n  traefik { disabled: true }\n}\n").unwrap_err();
    assert!(
        matches!(err, ParseError::UnexpectedToken { .. }),
        "got {err:?}"
    );
}

/// `traefik` has no `primary_field` (see `schema::TRAEFIK`'s doc) — the
/// bare, brace-free `traefik disabled` spelling the motivating issue
/// (#159) first floated is rejected rather than parsed as sugar for
/// anything.
#[test]
fn traefik_bare_value_without_braces_is_rejected() {
    let err = parse("service s {\n  traefik disabled\n}\n").unwrap_err();
    match err {
        ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::LBrace),
            ..
        } => {}
        other => panic!("expected UnexpectedToken expecting `{{`, got {other:?}"),
    }
}

/// A second bare `disabled` is `DuplicateField`, the same regression
/// coverage `bool_flag_duplicate_is_error` gives `network`'s `external`.
#[test]
fn traefik_disabled_duplicate_is_error() {
    let err = parse("service s {\n  traefik {\n    disabled\n    disabled\n  }\n}\n").unwrap_err();
    assert!(matches!(
        err,
        ParseError::DuplicateField {
            type_name: "traefik",
            field: "disabled",
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

// --- `router` blocks (#184) ---

fn routers(service: &hl_parser::Service) -> &[hl_parser::Router] {
    &service.fields.routers
}

fn router_entrypoints(router: &hl_parser::Router) -> Vec<&str> {
    router.entrypoint.iter().map(Literal::text).collect()
}

fn router_prefixes(router: &hl_parser::Router) -> Vec<&str> {
    router.path_prefix.iter().map(Literal::text).collect()
}

fn router_middleware(router: &hl_parser::Router) -> Vec<&str> {
    router.middleware.iter().map(Literal::text).collect()
}

/// The canonical form: a name after the keyword, then a braced body
/// whose fields are newline-separated like any other struct body.
#[test]
fn router_named_braced_body_parses() {
    let program = parse_ok(
        "service s {\n  image \"x\"\n  router api {\n    host: \"a.example.com\"\n    \
         entrypoint: web-secure\n    path_prefix: [\"/api/v1\", \"/dav/\"]\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let routers = routers(service);
    assert_eq!(routers.len(), 1);
    assert_eq!(routers[0].key(), Some("api"));
    assert_eq!(routers[0].host.as_ref().unwrap().text(), "a.example.com");
    assert_eq!(router_entrypoints(&routers[0]), vec!["web-secure"]);
    assert_eq!(router_prefixes(&routers[0]), vec!["/api/v1", "/dav/"]);
}

/// Leaving the name off is legal and means the router id `expose.host`
/// would have produced — codegen is what refuses writing both.
#[test]
fn router_unnamed_braced_body_parses() {
    let program = parse_ok("service s {\n  router { host: \"a.example.com\" }\n}\n");
    let service = as_service(&program.decls[0]);
    assert_eq!(routers(service).len(), 1);
    assert_eq!(routers(service)[0].key(), None);
}

/// The comma-continued spelling: the same secondary-field production a
/// primary-value shorthand continues from (see
/// `Parser::parse_secondary_fields`), here continuing from the router's
/// name instead of from a primary value.
#[test]
fn router_comma_shorthand_parses() {
    let program = parse_ok(
        "service s {\n  router api, host: \"a.example.com\", entrypoint: web-secure, \
         path_prefix: [\"/api\"]\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let routers = routers(service);
    assert_eq!(routers[0].key(), Some("api"));
    assert_eq!(routers[0].host.as_ref().unwrap().text(), "a.example.com");
    assert_eq!(router_entrypoints(&routers[0]), vec!["web-secure"]);
    assert_eq!(router_prefixes(&routers[0]), vec!["/api"]);
}

/// The shorthand form has no closing brace to end its span, so the
/// parser stretches it to the last token the secondary-field loop
/// consumed. Without that, the block's span would stop at the `router`
/// keyword and every diagnostic about the router would underline the
/// keyword alone rather than the fields that caused it.
#[test]
fn router_comma_shorthand_span_reaches_its_last_field() {
    let program =
        parse_ok("service s {\n  router api, host: \"a.example.com\", entrypoint: web-secure\n}\n");
    let service = as_service(&program.decls[0]);
    let router = &routers(service)[0];
    // The span starts at the `router` keyword, before the name...
    assert!(router.span.start < router.name.as_ref().unwrap().span.start);
    // ...and reaches past the host, out to the final `entrypoint` entry.
    assert!(router.span.end > router.host.as_ref().unwrap().span().end);
    assert_eq!(
        router.span.end,
        router.entrypoint.last().unwrap().span().end
    );
}

/// The braced form ends at its own closing brace, so its span reaches
/// past the last field for a different reason. Pinned alongside the
/// shorthand so the two paths can't drift apart.
#[test]
fn router_braced_body_span_reaches_past_its_last_field() {
    let program = parse_ok("service s {\n  router api {\n    host: \"a.example.com\"\n  }\n}\n");
    let service = as_service(&program.decls[0]);
    let router = &routers(service)[0];
    assert!(router.span.start < router.name.as_ref().unwrap().span.start);
    assert!(router.span.end > router.host.as_ref().unwrap().span().end);
}

/// Several blocks in one body accumulate in source order, which is what
/// makes the emitted label order a function of the source.
#[test]
fn several_routers_accumulate_in_source_order() {
    let program = parse_ok(
        "service s {\n  router api, host: \"a.example.com\"\n  \
         router web, host: \"b.example.com\"\n  router lan, host: \"c.example.com\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
    let names: Vec<Option<&str>> = routers(service).iter().map(|r| r.key()).collect();
    assert_eq!(names, vec![Some("api"), Some("web"), Some("lan")]);
}

/// `path_prefix` takes the bare comma-list sugar every list field takes,
/// and accumulates across repeats of the field.
#[test]
fn router_path_prefix_accepts_a_bare_list_and_accumulates() {
    let program = parse_ok(
        "service s {\n  router api {\n    host: \"a.example.com\"\n    \
         path_prefix: \"/api\", \"/dav\"\n    path_prefix: \"/.well-known\"\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(
        router_prefixes(&routers(service)[0]),
        vec!["/api", "/dav", "/.well-known"]
    );
}

/// The same `KEY :` one-token lookahead that keeps `expose`'s own bare
/// lists from swallowing a sibling field: the second comma here starts
/// `entrypoint`, not a third prefix.
#[test]
fn router_bare_path_prefix_list_ends_at_the_next_field() {
    let program = parse_ok(
        "service s {\n  router api, host: \"a.example.com\", path_prefix: \"/api\", \
         entrypoint: web-secure\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(router_prefixes(&routers(service)[0]), vec!["/api"]);
    assert_eq!(router_entrypoints(&routers(service)[0]), vec!["web-secure"]);
}

/// Two blocks claiming one router id are one router described twice,
/// with the later silently winning — refused, exactly as two `volume`
/// entries at one container path are.
#[test]
fn duplicate_router_name_is_rejected() {
    let err = parse(
        "service s {\n  router api, host: \"a.example.com\"\n  router api, host: \"b.example.com\"\n}\n",
    )
    .expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::DuplicateRouterName { ref name, .. } if name.as_deref() == Some("api")
        ),
        "got {err:?}"
    );
}

/// Including two unnamed blocks, which share the service's own id.
#[test]
fn duplicate_unnamed_router_is_rejected() {
    let err = parse(
        "service s {\n  router { host: \"a.example.com\" }\n  router { host: \"b.example.com\" }\n}\n",
    )
    .expect_err("expected a parse error");
    assert!(
        matches!(err, ParseError::DuplicateRouterName { name: None, .. }),
        "got {err:?}"
    );
}

/// Two routers with *different* names are the whole point of the field
/// and stay accepted.
#[test]
fn two_differently_named_routers_are_accepted() {
    let program = parse_ok(
        "service s {\n  router api, host: \"a.example.com\"\n  router web, host: \"b.example.com\"\n}\n",
    );
    assert_eq!(routers(as_service(&program.decls[0])).len(), 2);
}

/// A template body accepts `router` exactly as a service body does —
/// the two share one field list.
#[test]
fn router_parses_in_a_template_body() {
    let program = parse_ok("template t {\n  router api, host: \"a.example.com\"\n}\n");
    let template = as_template(&program.decls[0]);
    assert_eq!(template.fields.routers.len(), 1);
}

/// A `$param` is legal in a `router`'s `host` and in each
/// `path_prefix` — which is why `path_prefix` holds literals rather than
/// references, since a reference has no `$param` form at all.
#[test]
fn router_host_and_path_prefix_accept_a_param() {
    let program =
        parse_ok("template t(h, p) {\n  router api { host: $h\n    path_prefix: [$p] }\n}\n");
    let template = as_template(&program.decls[0]);
    let router = &template.fields.routers[0];
    assert!(matches!(
        router.host.as_ref().unwrap(),
        Literal::Param(name, _) if name == "h"
    ));
    assert!(matches!(&router.path_prefix[0], Literal::Param(name, _) if name == "p"));
}

/// An unknown sub-field is refused against `router`'s own field list,
/// which is what keeps a typo from being silently dropped.
#[test]
fn unknown_router_field_is_rejected() {
    let err = parse("service s {\n  router api { bogus: 1 }\n}\n").expect_err("expected an error");
    assert!(
        matches!(
            err,
            ParseError::UnknownField {
                type_name: "router",
                ref field,
                ..
            } if field == "bogus"
        ),
        "got {err:?}"
    );
}

/// The unnamed form needs its braces: with no name and no `{`, there is
/// no first token to continue a comma-list from, and `router host: "x"`
/// would have to guess whether `host` names the router or its own field.
#[test]
fn unnamed_router_without_a_body_is_rejected() {
    let err = parse("service s {\n  router\n}\n").expect_err("expected a parse error");
    assert!(
        matches!(err, ParseError::UnexpectedToken { .. }),
        "got {err:?}"
    );
}

/// Writing `host` twice in one router body is the ordinary duplicate
/// scalar error, reported against `router` rather than the enclosing
/// service.
#[test]
fn duplicate_router_host_is_rejected() {
    let err = parse(
        "service s {\n  router api {\n    host: \"a.example.com\"\n    host: \"b.example.com\"\n  }\n}\n",
    )
    .expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::DuplicateField {
                type_name: "router",
                field: "host",
                ..
            }
        ),
        "got {err:?}"
    );
}

// --- per-router `middleware` (#221) ---

/// The field this issue asked for: a `router` block carrying its own
/// middleware list, resolved against `router`'s own field table — the
/// only place `middleware` is a field at all since #221.
#[test]
fn router_middleware_parses_in_a_braced_body() {
    let program = parse_ok(
        "service s {\n  router internal {\n    host: \"a.example.com\"\n    \
         middleware: local-ipwhitelist\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(
        router_middleware(&routers(service)[0]),
        vec!["local-ipwhitelist"]
    );
}

/// It takes the bare comma-list sugar and the bracketed form every
/// reference list takes, and accumulates across repeats of the field.
#[test]
fn router_middleware_accepts_a_bare_list_and_accumulates() {
    let program = parse_ok(
        "service s {\n  router internal {\n    host: \"a.example.com\"\n    \
         middleware: local-ipwhitelist, forwardAuth-authentik\n    \
         middleware: [rate-limit]\n  }\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(
        router_middleware(&routers(service)[0]),
        vec!["local-ipwhitelist", "forwardAuth-authentik", "rate-limit"]
    );
}

/// And the comma-continued spelling, where the same `KEY :` one-token
/// lookahead that bounds `path_prefix`'s bare list bounds this one.
#[test]
fn router_middleware_in_comma_shorthand_ends_at_the_next_field() {
    let program = parse_ok(
        "service s {\n  router internal, middleware: local-ipwhitelist, \
         host: \"a.example.com\"\n}\n",
    );
    let service = as_service(&program.decls[0]);
    assert_eq!(
        router_middleware(&routers(service)[0]),
        vec!["local-ipwhitelist"]
    );
    assert_eq!(
        routers(service)[0].host.as_ref().unwrap().text(),
        "a.example.com"
    );
}

/// A `$param` reaches it like every other reference-shaped position
/// (#196), so a template can parameterize which middleware one of its
/// routers attaches.
#[test]
fn router_middleware_accepts_a_param() {
    let program = parse_ok("template t(mw) {\n  router api { middleware: [$mw] }\n}\n");
    let template = as_template(&program.decls[0]);
    let router = &template.fields.routers[0];
    assert!(matches!(&router.middleware[0], Literal::Param(name, _) if name == "mw"));
}

/// The qualified form parses here, exactly as it does in every other
/// reference-shaped position — `compose()` is what rejects it, since a
/// middleware has no `.hll` declaration for an alias to resolve against.
#[test]
fn qualified_router_middleware_reference_parses() {
    let program = parse_ok("service s {\n  router api { middleware: common.forwardAuth }\n}\n");
    let service = as_service(&program.decls[0]);
    let r = &routers(service)[0].middleware[0];
    assert_eq!(r.qualifier().unwrap().name, "common");
    assert_eq!(r.text(), "forwardAuth");
}

/// The old service-level spelling is refused outright rather than
/// silently ignored, and the diagnostic says where the field went — a
/// bare `UnknownField` here would offer `raw { middleware: ... }`,
/// which compiles and emits a meaningless Compose key while the Traefik
/// label the author wanted goes missing.
#[test]
fn service_level_middleware_names_its_new_home() {
    let err = parse("service s {\n  image \"x\"\n  middleware forwardAuth-authentik\n}\n")
        .expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::MovedField {
                type_name: "service",
                ref field,
                ..
            } if field == "middleware"
        ),
        "got {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "3:3: `middleware` is no longer a `service` field — move it inside the `router` block \
         it applies to (`router { host: \"...\", middleware: ... }`)"
    );
}

/// A `template` body shares `service`'s field list, so it reports the
/// move the same way — named for `template`, since that's the body the
/// line was written in.
#[test]
fn template_level_middleware_names_its_new_home_too() {
    let err = parse("template t {\n  middleware auth\n}\n").expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::MovedField {
                type_name: "template",
                ..
            }
        ),
        "got {err:?}"
    );
}

/// The move is reported even in the comma-continued position, where the
/// lookahead has to decide whether `middleware` continues the preceding
/// field's list. It doesn't: a moved name is no more a continuation
/// than an unknown one, so the error names the enclosing body rather
/// than the `router` the comma came from.
#[test]
fn service_level_middleware_after_a_comma_is_still_reported() {
    let err = parse("service s {\n  router api, host: \"a.example.com\"\n  middleware auth\n}\n")
        .expect_err("expected a parse error");
    assert!(
        matches!(
            err,
            ParseError::MovedField {
                type_name: "service",
                ..
            }
        ),
        "got {err:?}"
    );
}
