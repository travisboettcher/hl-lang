//! Integration tests: parse the design doc's worked examples end-to-end
//! and assert the resulting AST's content field-by-field.

use hl_parser::{ComposeError, Literal, TopDecl, compose, parse};

const JELLYFIN: &str = include_str!("fixtures/jellyfin.hll");
const SYNCTHING: &str = include_str!("fixtures/syncthing.hll");
const NETWORK: &str = include_str!("fixtures/network.hll");
const RAW_SERVICE: &str = include_str!("fixtures/raw_service.hll");
const UNKNOWN_TEMPLATE: &str = include_str!("fixtures/unknown_template.hll");

#[test]
fn jellyfin_fixture_parses_to_expected_ast() {
    let program = parse(JELLYFIN).expect("jellyfin.hll should parse");
    assert_eq!(program.decls.len(), 1);
    let TopDecl::Service(service) = &program.decls[0] else {
        panic!("expected a Service decl");
    };
    assert_eq!(service.name.name, "jellyfin");

    let image = service.fields.image.as_ref().expect("image field set");
    assert_eq!(
        image.reference.as_ref().unwrap().text(),
        "jellyfin/jellyfin:latest"
    );

    let expose = service.fields.expose.as_ref().expect("expose field set");
    assert_eq!(expose.port.as_ref().unwrap().text(), "8096");
    assert_eq!(expose.host.as_ref().unwrap().text(), "media.techdebtor.io");

    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(service.fields.volumes.entries[0].host.text(), "/mnt/media");
    assert_eq!(service.fields.volumes.entries[0].container.text(), "/data");

    assert_eq!(service.fields.env.entries.len(), 1);
    assert_eq!(service.fields.env.entries[0].key.text(), "PUID");
    assert_eq!(service.fields.env.entries[0].value.text(), "1000");

    let restart = service.fields.restart.as_ref().expect("restart field set");
    assert_eq!(restart.policy.as_ref().unwrap().text(), "unless-stopped");

    assert!(service.fields.raw.entries.is_empty());
    assert!(service.fields.middleware.is_empty());
    assert!(service.fields.depends_on.is_empty());
    assert!(service.fields.networks.is_empty());
    assert!(service.fields.with.is_empty());
}

/// The design doc's canonical composition worked example: three
/// templates (`internal_web`, `authenticated`, `linuxserver_app`) merged
/// onto `service syncthing` via `with`. Parses, then composes, and
/// checks the fully-resolved service field-by-field — including that
/// argument substitution preserves the *argument's* literal kind (the
/// template body writes `env PUID = $puid` with a `$`-sigiled parameter
/// reference, but the resolved value is a `Literal::Number`, since that's
/// what `puid: 1000` supplied) and that middleware/networks accumulate in
/// with-list left-to-right order.
#[test]
fn syncthing_fixture_composes_to_expected_service() {
    let program = parse(SYNCTHING).expect("syncthing.hll should parse");
    let composed = compose(program).expect("syncthing.hll should compose");
    assert_eq!(composed.services.len(), 1);
    let service = &composed.services[0];
    assert_eq!(service.name.name, "syncthing");

    // Own body always wins: image/volume are set directly on the
    // service, not by any template.
    let image = service.fields.image.as_ref().expect("image field set");
    assert_eq!(
        image.reference.as_ref().unwrap().text(),
        "lscr.io/linuxserver/syncthing:latest"
    );
    assert_eq!(service.fields.volumes.entries.len(), 1);
    assert_eq!(
        service.fields.volumes.entries[0].host.text(),
        "syncthing-config"
    );
    assert_eq!(
        service.fields.volumes.entries[0].container.text(),
        "/config"
    );

    // `internal_web`'s contributions.
    let expose = service.fields.expose.as_ref().expect("expose field set");
    let port = expose.port.as_ref().unwrap();
    assert_eq!(port.text(), "8384");
    assert!(
        matches!(port, Literal::Number { .. }),
        "expose.port should be substituted with the invocation's Number argument, got {port:?}"
    );
    assert_eq!(
        expose.host.as_ref().unwrap().text(),
        "{{name}}.internal.techdebtor.io"
    );
    assert_eq!(expose.entrypoint.as_ref().unwrap().text(), "web-secure");
    let restart = service.fields.restart.as_ref().expect("restart field set");
    assert_eq!(restart.policy.as_ref().unwrap().text(), "unless-stopped");
    assert_eq!(
        service
            .fields
            .networks
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["traefik-net"]
    );

    // `internal_web` then `authenticated`, left-to-right.
    assert_eq!(
        service
            .fields
            .middleware
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["local-ipwhitelist", "forwardAuth-authentik"]
    );

    // `linuxserver_app`'s contributions: PUID/PGID values were written
    // as `$`-sigiled parameter references (`$puid`/`$pgid`) in the
    // template body but must come out as the Number arguments the
    // invocation supplied.
    assert_eq!(service.fields.env.entries.len(), 2);
    let puid = &service.fields.env.entries[0];
    assert_eq!(puid.key.text(), "PUID");
    assert_eq!(puid.value.text(), "1000");
    assert!(matches!(puid.value, Literal::Number { .. }));
    let pgid = &service.fields.env.entries[1];
    assert_eq!(pgid.key.text(), "PGID");
    assert_eq!(pgid.value.text(), "100");
    assert!(matches!(pgid.value, Literal::Number { .. }));

    assert!(service.fields.with.is_empty());
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
    assert_eq!(service.fields.raw.entries.len(), 3);

    let keys: Vec<&str> = service
        .fields
        .raw
        .entries
        .iter()
        .map(|e| e.key.text())
        .collect();
    assert_eq!(keys, vec!["privileged", "devices", "security_opt"]);

    match &service.fields.raw.entries[1].value {
        hl_parser::RawValue::List(items, _) => assert_eq!(items.len(), 1),
        other => panic!("expected `devices` to be a raw list, got {other:?}"),
    }
    match &service.fields.raw.entries[2].value {
        hl_parser::RawValue::Map(entries, _) => assert_eq!(entries.len(), 1),
        other => panic!("expected `security_opt` to be a nested raw map, got {other:?}"),
    }
}

/// This fixture has no top-level `template` declaration at all, so its
/// `with`-list references templates that don't exist anywhere in the
/// program. Parsing must succeed (`with` is ordinary syntax now) —
/// composition is what must fail.
#[test]
fn unknown_template_fixture_fails_compose_with_unknown_template() {
    let program = parse(UNKNOWN_TEMPLATE).expect("unknown_template.hll should parse");
    let err = compose(program).expect_err("unknown_template.hll should fail to compose");
    assert!(matches!(
        err,
        ComposeError::UnknownTemplate { name, .. } if name == "internal_web"
    ));
}
