//! Computes one service's Traefik labels. These live on the service's
//! own `labels:` list, not a separate config file — Traefik's Docker
//! provider reads labels off container metadata directly, confirmed
//! against every real Docker-based service in the homelab this targets.

use std::collections::HashMap;

use hl_parser::ServiceFields;

use crate::{CodegenError, interp};

/// Computes `service_name`'s Traefik label list, in this order:
/// `traefik.docker.network=` (if `docker_network` is set — the real name
/// of whichever of the service's declared networks is `external`),
/// the router rule (from `expose.host`), `.entrypoints=` (if
/// `expose.entrypoint` is set), `.middlewares=` (if any, each getting an
/// `@file` suffix — the file provider's own reference convention,
/// confirmed mechanical/always-on, not homelab-specific), and finally
/// the loadbalancer port (if `expose.port` is set) — emitted whenever a
/// port is set, even when technically redundant with Traefik's
/// single-port default, matching every real example's own "always
/// explicit" convention.
pub fn compute(
    service_name: &str,
    fields: &ServiceFields,
    docker_network: Option<&str>,
    bindings: &HashMap<&str, &str>,
) -> Result<Vec<String>, CodegenError> {
    let mut labels = Vec::new();

    if let Some(net) = docker_network {
        labels.push(format!("traefik.docker.network={net}"));
    }

    let Some(expose) = &fields.expose else {
        return Ok(labels);
    };
    let Some(host_lit) = &expose.host else {
        return Ok(labels);
    };
    let host = interp::resolve(host_lit.text(), bindings, host_lit.span())?;
    labels.push(format!(
        "traefik.http.routers.{service_name}.rule=Host(`{host}`)"
    ));

    if let Some(ep) = &expose.entrypoint {
        let entrypoint = interp::resolve(ep.text(), bindings, ep.span())?;
        labels.push(format!(
            "traefik.http.routers.{service_name}.entrypoints={entrypoint}"
        ));
    }

    if !fields.middleware.is_empty() {
        let mws: Vec<String> = fields
            .middleware
            .iter()
            .map(|r| format!("{}@file", r.name))
            .collect();
        labels.push(format!(
            "traefik.http.routers.{service_name}.middlewares={}",
            mws.join(",")
        ));
    }

    if let Some(port) = &expose.port {
        labels.push(format!(
            "traefik.http.services.{service_name}.loadbalancer.server.port={}",
            port.text()
        ));
    }

    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_parser::Span;
    use hl_parser::{Expose, Literal, Reference};

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        }
    }

    fn lit(text: &str) -> Literal {
        Literal::Str(text.to_string(), span())
    }

    fn bindings() -> HashMap<&'static str, &'static str> {
        HashMap::from([("name", "syncthing")])
    }

    #[test]
    fn no_expose_means_no_router_labels() {
        let fields = ServiceFields::default();
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(labels.is_empty());
    }

    #[test]
    fn docker_network_label_when_present() {
        let fields = ServiceFields::default();
        let labels = compute("s", &fields, Some("docker_default"), &bindings()).unwrap();
        assert_eq!(labels, vec!["traefik.docker.network=docker_default"]);
    }

    #[test]
    fn host_only_produces_rule_but_no_entrypoints_label() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: None,
                host: Some(lit("syncthing.internal.techdebtor.io")),
                entrypoint: None,
                span: span(),
            }),
            ..Default::default()
        };
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)"]
        );
    }

    #[test]
    fn full_expose_produces_all_router_labels_in_order() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: Some(Literal::Number {
                    text: "8384".to_string(),
                    value: 8384,
                    span: span(),
                }),
                host: Some(lit("syncthing.internal.techdebtor.io")),
                entrypoint: Some(lit("web-secure")),
                span: span(),
            }),
            middleware: vec![
                Reference {
                    qualifier: None,
                    name: "local-ipwhitelist".to_string(),
                    name_span: span(),
                    span: span(),
                },
                Reference {
                    qualifier: None,
                    name: "forwardAuth-authentik".to_string(),
                    name_span: span(),
                    span: span(),
                },
            ],
            ..Default::default()
        };
        let labels = compute("syncthing", &fields, Some("docker_default"), &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.docker.network=docker_default",
                "traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)",
                "traefik.http.routers.syncthing.entrypoints=web-secure",
                "traefik.http.routers.syncthing.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file",
                "traefik.http.services.syncthing.loadbalancer.server.port=8384",
            ]
        );
    }
}
