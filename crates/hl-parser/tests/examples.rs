//! Integration tests: parse the design doc's worked examples end-to-end
//! and assert the resulting AST's content field-by-field.

use hl_parser::{ParseError, TopDecl, parse};

const JELLYFIN: &str = include_str!("fixtures/jellyfin.hll");
const SYNCTHING: &str = include_str!("fixtures/syncthing.hll");
const NETWORK: &str = include_str!("fixtures/network.hll");
const RAW_SERVICE: &str = include_str!("fixtures/raw_service.hll");
const WITH_REJECTED: &str = include_str!("fixtures/with_rejected.hll");

#[test]
fn jellyfin_fixture_parses_to_expected_ast() {
    let program = parse(JELLYFIN).expect("jellyfin.hll should parse");
    assert_eq!(program.decls.len(), 1);
    let TopDecl::Service(service) = &program.decls[0] else {
        panic!("expected a Service decl");
    };
    assert_eq!(service.name.name, "jellyfin");

    let image = service.image.as_ref().expect("image field set");
    assert_eq!(
        image.reference.as_ref().unwrap().text(),
        "jellyfin/jellyfin:latest"
    );

    let expose = service.expose.as_ref().expect("expose field set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
    assert_eq!(expose.host.as_ref().unwrap().text(), "media.techdebtor.io");

    assert_eq!(service.volumes.entries.len(), 1);
    assert_eq!(service.volumes.entries[0].host.text(), "/mnt/media");
    assert_eq!(service.volumes.entries[0].container.text(), "/data");

    assert_eq!(service.env.entries.len(), 1);
    assert_eq!(service.env.entries[0].key.text(), "PUID");
    assert_eq!(service.env.entries[0].value.text(), "1000");

    let restart = service.restart.as_ref().expect("restart field set");
    assert_eq!(restart.policy.as_ref().unwrap().text(), "unless-stopped");

    assert!(service.raw.entries.is_empty());
    assert!(service.middleware.is_empty());
    assert!(service.depends_on.is_empty());
    assert!(service.networks.is_empty());
}

/// This fixture uses `template`/`with` composition, which this milestone
/// deliberately does not support (see docs/DESIGN.md's Pipeline section).
/// It's now the golden "not yet supported" case rather than a golden AST
/// — parsing must fail at the very first `template` token, not panic or
/// silently produce a partial AST.
#[test]
fn syncthing_fixture_is_template_not_supported_error() {
    let err =
        parse(SYNCTHING).expect_err("syncthing.hll uses templates, which aren't supported yet");
    match err {
        ParseError::TemplatesNotSupported {
            what: "template declaration",
            span,
        } => {
            // The fixture's first non-comment line is the first `template` block.
            assert_eq!(span.line, 4);
        }
        other => {
            panic!("expected TemplatesNotSupported at the first `template` token, got {other:?}")
        }
    }
}

#[test]
fn network_fixture_parses_to_expected_ast() {
    let program = parse(NETWORK).expect("network.hll should parse");
    assert_eq!(program.decls.len(), 2);

    let TopDecl::Network(traefik_net) = &program.decls[0] else {
        panic!("expected a Network decl")
    };
    assert_eq!(traefik_net.name.name, "traefik-net");
    assert!(traefik_net.external.is_some());

    let TopDecl::Network(internal) = &program.decls[1] else {
        panic!("expected a Network decl")
    };
    assert_eq!(internal.name.name, "internal");
    assert!(internal.external.is_none());
}

#[test]
fn raw_service_fixture_parses_to_expected_ast() {
    let program = parse(RAW_SERVICE).expect("raw_service.hll should parse");
    let TopDecl::Service(service) = &program.decls[0] else {
        panic!("expected a Service decl")
    };
    assert_eq!(service.name.name, "cadvisor");
    assert_eq!(service.raw.entries.len(), 3);

    let keys: Vec<&str> = service.raw.entries.iter().map(|e| e.key.text()).collect();
    assert_eq!(keys, vec!["privileged", "devices", "security_opt"]);

    match &service.raw.entries[1].value {
        hl_parser::RawValue::List(items, _) => assert_eq!(items.len(), 1),
        other => panic!("expected `devices` to be a raw list, got {other:?}"),
    }
    match &service.raw.entries[2].value {
        hl_parser::RawValue::Map(entries, _) => assert_eq!(entries.len(), 1),
        other => panic!("expected `security_opt` to be a nested raw map, got {other:?}"),
    }
}

/// Isolates the `with`-as-a-field error from the top-level `template`
/// token error: this fixture has no top-level `template` declaration at
/// all, so the error must come specifically from `with` being used as a
/// field inside the service body.
#[test]
fn with_rejected_fixture_is_not_supported_error() {
    let err =
        parse(WITH_REJECTED).expect_err("with_rejected.hll uses `with`, which isn't supported yet");
    assert!(matches!(
        err,
        ParseError::TemplatesNotSupported {
            what: "the `with` field",
            ..
        }
    ));
}
