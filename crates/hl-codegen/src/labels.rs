//! Computes one service's Traefik labels. These live on the service's
//! own `labels:` list, not a separate config file — Traefik's Docker
//! provider reads labels off container metadata directly, confirmed
//! against every real Docker-based service in the homelab this targets.

use std::collections::HashMap;

use hl_parser::{ServiceFields, Span};

use crate::{CodegenError, interp};

/// Characters rejected in every label value the user writes directly —
/// `expose.host` and each `expose.entrypoint` entry. Motivated by
/// `expose.host`, which is spliced verbatim into
/// a ``Host(`...`)`` router rule. A backtick alone is enough to break
/// out: it closes the value and everything after it is read as more
/// rule grammar, so `` host "ok.example.com`) || HostRegexp(`{any:.+}" ``
/// compiles to a valid rule matching *every* host (#65). Traefik's rule
/// syntax has no backtick escape, so the only available answer is to
/// reject.
///
/// The rest of the set closes the same door earlier: each is live rule
/// syntax (grouping, `||`/`&&`, the `{name:regexp}` capture form,
/// comma-separated matcher arguments, quoting) and none of them can
/// appear in a real hostname — deliberately a short list of
/// clearly-dangerous characters rather than an attempt at a full
/// hostname grammar, so no legitimate existing config is broken.
///
/// `expose.entrypoint` shares this exact set, `,` included. It used to
/// need its own copy with `,` carved out, because a single scalar
/// `entrypoint "web,websecure"` was the only way to attach a router to
/// more than one entry point. Now that `entrypoint` is a list and
/// codegen writes the separator itself, a comma *inside* one entry can
/// only ever be a mistake — so the carve-out is gone and one set covers
/// every label value, with no per-field exception to keep in mind.
const LABEL_METACHARACTERS: &[char] = &['`', '(', ')', '{', '}', '|', '&', ',', '"', '\'', '\\'];

/// `middlewares=` is one comma-joined label (see [`compute`]), so a
/// comma *inside* a single middleware name splices extra entries into
/// it: `middleware ["a,b"]` produced `middlewares=a,b@file`, i.e. two
/// references, only the second of which got the `@file` suffix (#65).
const MIDDLEWARE_METACHARACTERS: &[char] = &[','];

/// Rejects `value` if it contains any of `forbidden`. Always applied to
/// the *resolved* value, after `{{}}` interpolation, since that's the
/// text that actually reaches the label.
fn reject_metacharacters(
    value: &str,
    field: &'static str,
    forbidden: &[char],
    span: Span,
) -> Result<(), CodegenError> {
    match value.chars().find(|c| forbidden.contains(c)) {
        Some(character) => Err(CodegenError::UnsafeLabelValue {
            field,
            character,
            span,
        }),
        None => Ok(()),
    }
}

/// The first field a service sets that only means anything attached to a
/// Traefik router, if it sets one at all — in the order [`compute`]
/// would have emitted their labels, so the field reported is the first
/// one that needed the missing `expose.host`.
///
/// Both are pointed at by their first entry's own span rather than the
/// field's: a repeated `middleware` accumulates entries across a
/// template and the service's own body, and the first one is the entry
/// that the merged service leads with.
fn router_field_needing_host(fields: &ServiceFields) -> Option<(&'static str, Span)> {
    fields
        .expose
        .as_ref()
        .and_then(|e| e.entrypoint.first())
        .map(|r| ("expose.entrypoint", r.name_span))
        .or_else(|| {
            fields
                .middleware
                .first()
                .map(|r| ("middleware", r.name_span))
        })
}

/// The first Traefik-specific field a *disabled* service also sets, if
/// it sets one at all (#159) — checked in the same order [`compute`]
/// would have emitted their labels were the service not disabled
/// (`expose.host`, then `expose.entrypoint`, then `middleware`), so the
/// field named in the diagnostic is the first one that contradicts
/// `traefik { disabled }`.
///
/// `expose.port` deliberately isn't checked here: it's Compose's own
/// `expose:` key, plain container-network visibility with nothing to do
/// with Traefik, so a disabled service may still declare it (#159) —
/// see [`crate::CodegenError::TraefikDisabledWithRouterField`]'s doc.
/// The other three all reuse [`router_field_needing_host`] for
/// `expose.entrypoint`/`middleware`, since a disabled service's
/// contradiction is checked against exactly the same two router-attached
/// fields the router-less-middleware check (#144) already knows about —
/// only `expose.host` itself is new here, since disabling Traefik makes
/// even the host that would have *created* the router a contradiction,
/// not just the fields that would attach to it.
fn traefik_conflict_field(fields: &ServiceFields) -> Option<(&'static str, Span)> {
    fields
        .expose
        .as_ref()
        .and_then(|e| e.host.as_ref())
        .map(|host| ("expose.host", host.span()))
        .or_else(|| router_field_needing_host(fields))
}

/// Computes `service_name`'s Traefik label list, in this order:
/// `traefik.docker.network=` (if `docker_network` is set — the real name
/// of whichever of the service's declared networks is `external`),
/// the router rule (from `expose.host`), `.entrypoints=` (if
/// `expose.entrypoint` is non-empty — one comma-joined label for the
/// whole list, the same shape as `.middlewares=` below but with no
/// `@file` suffix, which is a file-provider convention specific to
/// middleware references), `.middlewares=` (if any, each getting an
/// `@file` suffix — the file provider's own reference convention,
/// confirmed mechanical/always-on, not homelab-specific), and finally
/// the loadbalancer port (if `expose.port` is set) — emitted whenever a
/// port is set, even when technically redundant with Traefik's
/// single-port default, matching every real example's own "always
/// explicit" convention.
///
/// Both router-attached fields — `expose.entrypoint` and `middleware` —
/// require `expose.host`, since that's what creates the router they'd
/// attach to. Setting either without a host is
/// [`CodegenError::RouterFieldWithoutHost`], not a silently label-less
/// service (#80).
///
/// A service that sets `traefik { disabled }` (#159) short-circuits all
/// of the above: the returned list is exactly `["traefik.enable=false"]`,
/// full stop — no `traefik.docker.network=` label either. That label
/// only matters for routing traffic *to* the container once Traefik's
/// Docker provider has decided to act on it; with `enable=false` it
/// never acts on the container at all, so the network label would be
/// dead configuration, and leaving it out keeps "disabled" reading as
/// exactly one label rather than one meaningful label plus one inert
/// one. `raw { labels: [...] }` still overrides this entirely, same as
/// it overrides the ordinary computed list — see
/// [`crate::doc::ComposeServiceDoc::apply_raw_overrides`].
pub fn compute(
    service_name: &str,
    fields: &ServiceFields,
    docker_network: Option<&str>,
    bindings: &HashMap<&str, &str>,
) -> Result<Vec<String>, CodegenError> {
    if let Some(traefik) = &fields.traefik
        && let Some(disabled_span) = traefik.disabled
    {
        if let Some((field, span)) = traefik_conflict_field(fields) {
            return Err(CodegenError::TraefikDisabledWithRouterField {
                service: service_name.to_string(),
                field,
                disabled_span,
                span,
            });
        }
        return Ok(vec!["traefik.enable=false".to_string()]);
    }

    let mut labels = Vec::new();

    if let Some(net) = docker_network {
        labels.push(format!("traefik.docker.network={net}"));
    }

    // Checked before either binding below, so "no `expose` block at all"
    // and "an `expose` block with no `host`" take the same path — a
    // `middleware` line is equally router-less either way.
    if fields
        .expose
        .as_ref()
        .and_then(|e| e.host.as_ref())
        .is_none()
        && let Some((field, span)) = router_field_needing_host(fields)
    {
        return Err(CodegenError::RouterFieldWithoutHost {
            service: service_name.to_string(),
            field,
            span,
        });
    }

    let Some(expose) = &fields.expose else {
        return Ok(labels);
    };
    let Some(host_lit) = &expose.host else {
        return Ok(labels);
    };
    let host = interp::resolve(host_lit.text(), bindings, host_lit.span())?;
    reject_metacharacters(&host, "expose.host", LABEL_METACHARACTERS, host_lit.span())?;
    labels.push(format!(
        "traefik.http.routers.{service_name}.rule=Host(`{host}`)"
    ));

    if !expose.entrypoint.is_empty() {
        // Interpolation still runs per entry, before validation, for
        // the same reason it always has: `{{name}}` is resolved here, so
        // the resolved text is what actually reaches the label and
        // therefore what the metacharacter guard has to inspect. A
        // reference spelled as a string (`entrypoint "{{name}}-secure"`)
        // is the case that makes this observable.
        let mut eps = Vec::with_capacity(expose.entrypoint.len());
        for r in &expose.entrypoint {
            let entrypoint = interp::resolve(&r.name, bindings, r.name_span)?;
            reject_metacharacters(
                &entrypoint,
                "expose.entrypoint",
                LABEL_METACHARACTERS,
                r.name_span,
            )?;
            eps.push(entrypoint);
        }
        labels.push(format!(
            "traefik.http.routers.{service_name}.entrypoints={}",
            eps.join(",")
        ));
    }

    if !fields.middleware.is_empty() {
        let mut mws = Vec::with_capacity(fields.middleware.len());
        for r in &fields.middleware {
            reject_metacharacters(
                &r.name,
                "middleware",
                MIDDLEWARE_METACHARACTERS,
                r.name_span,
            )?;
            mws.push(format!("{}@file", r.name));
        }
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
    use hl_parser::{Expose, Literal, Reference, Traefik};
    use hl_parser::{FileId, Span};

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
            file: FileId::ANONYMOUS,
        }
    }

    fn lit(text: &str) -> Literal {
        Literal::Str(text.to_string(), span())
    }

    fn bindings() -> HashMap<&'static str, &'static str> {
        HashMap::from([("name", "syncthing")])
    }

    fn refs(names: &[&str]) -> Vec<Reference> {
        names
            .iter()
            .map(|n| Reference {
                qualifier: None,
                name: (*n).to_string(),
                name_span: span(),
                span: span(),
            })
            .collect()
    }

    #[test]
    fn no_expose_means_no_router_labels() {
        let fields = ServiceFields::default();
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(labels.is_empty());
    }

    /// #80: a `middleware` with no router to attach to is refused rather
    /// than dropped — and refused whether the service has a host-less
    /// `expose` block or no `expose` block at all.
    #[test]
    fn middleware_without_a_host_is_rejected() {
        let mut fields = ServiceFields {
            middleware: refs(&["forwardAuth-authentik"]),
            ..Default::default()
        };
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::RouterFieldWithoutHost {
                field: "middleware",
                ..
            }
        ));

        fields.expose = Some(Expose {
            port: Some(Literal::Number {
                text: "80".to_string(),
                value: 80,
                span: span(),
            }),
            host: None,
            entrypoint: Vec::new(),
            span: span(),
        });
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::RouterFieldWithoutHost {
                field: "middleware",
                ..
            }
        ));
    }

    /// `expose.entrypoint` needs the same router, and is the field
    /// reported when a service sets both — it's the one whose label would
    /// have been emitted first.
    #[test]
    fn entrypoint_without_a_host_is_rejected_before_middleware() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: None,
                host: None,
                entrypoint: refs(&["web-secure"]),
                span: span(),
            }),
            middleware: refs(&["forwardAuth-authentik"]),
            ..Default::default()
        };
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::RouterFieldWithoutHost {
                field: "expose.entrypoint",
                ..
            }
        ));
    }

    /// The guard is specific to the router-attached fields: a host-less
    /// service that sets neither still generates its non-router labels
    /// and no diagnostic.
    #[test]
    fn a_hostless_service_without_router_fields_is_fine() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: None,
                host: None,
                entrypoint: Vec::new(),
                span: span(),
            }),
            ..Default::default()
        };
        let labels = compute("s", &fields, Some("docker_default"), &bindings()).unwrap();
        assert_eq!(labels, vec!["traefik.docker.network=docker_default"]);
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
                entrypoint: Vec::new(),
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

    fn expose_with_host(host: &str) -> ServiceFields {
        ServiceFields {
            expose: Some(Expose {
                port: None,
                host: Some(lit(host)),
                entrypoint: Vec::new(),
                span: span(),
            }),
            ..Default::default()
        }
    }

    /// #65: the exact escape from the issue — a backtick closes the
    /// `Host()` value and the rest is read as more rule grammar,
    /// yielding a valid rule that matches every host.
    #[test]
    fn backtick_in_host_is_rejected() {
        let fields = expose_with_host("ok.example.com`) || HostRegexp(`{any:.+}");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.host",
                character: '`',
                ..
            }
        ));
    }

    #[test]
    fn comma_in_host_is_rejected() {
        let fields = expose_with_host("a.example.com,b.example.com");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.host",
                character: ',',
                ..
            }
        ));
    }

    /// The guard runs on the *resolved* value, so a metacharacter that
    /// only appears after `{{name}}` interpolation is still caught.
    #[test]
    fn host_is_checked_after_interpolation() {
        let fields = expose_with_host("{{name}}.example.com");
        let bindings = HashMap::from([("name", "bad`)")]);
        let err = compute("s", &fields, None, &bindings).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.host",
                character: '`',
                ..
            }
        ));
    }

    #[test]
    fn backtick_in_entrypoint_is_rejected() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["web`-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.entrypoint",
                character: '`',
                ..
            }
        ));
    }

    /// Codegen owns the comma that separates entry points, so a comma
    /// *inside* one entry can only splice an extra name into the joined
    /// label — the same failure mode `middleware` already guards
    /// against. This used to be the one accepted metacharacter here.
    #[test]
    fn comma_in_a_single_entrypoint_is_rejected() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["web,web-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.entrypoint",
                character: ',',
                ..
            }
        ));
    }

    /// The replacement for the old comma-in-a-scalar spelling: several
    /// entry points are several list entries, and codegen joins them
    /// into the one `entrypoints=` label Traefik expects — with no
    /// `@file` suffix, which is `middleware`'s convention alone.
    #[test]
    fn several_entrypoints_join_into_one_label() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["web", "web-secure"]);
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(
            labels
                .iter()
                .any(|l| l == "traefik.http.routers.s.entrypoints=web,web-secure"),
            "expected a joined entrypoints label, got {labels:?}"
        );
    }

    /// The list is joined, not emitted one label per entry — a
    /// per-entry label would silently overwrite itself in the Compose
    /// `labels:` map and leave only the last entry point attached.
    #[test]
    fn several_entrypoints_produce_exactly_one_entrypoints_label() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["web", "web-secure", "metrics"]);
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels
                .iter()
                .filter(|l| l.starts_with("traefik.http.routers.s.entrypoints="))
                .count(),
            1
        );
    }

    /// Same guarantee as `host_is_checked_after_interpolation`, for an
    /// entry point spelled as a string so it can carry a `{{name}}`.
    #[test]
    fn entrypoint_is_checked_after_interpolation() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["{{name}}-secure"]);
        let bindings = HashMap::from([("name", "web`")]);
        let err = compute("s", &fields, None, &bindings).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.entrypoint",
                character: '`',
                ..
            }
        ));
    }

    /// And the resolved text — not the raw `{{name}}` source — is what
    /// lands in the label.
    #[test]
    fn entrypoint_is_interpolated_into_the_label() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["{{name}}-secure"]);
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(
            labels
                .iter()
                .any(|l| l == "traefik.http.routers.s.entrypoints=syncthing-secure"),
            "expected an interpolated entrypoints label, got {labels:?}"
        );
    }

    /// #65: `middlewares=` is one comma-joined label, so a comma inside
    /// a single name splices an extra entry into it.
    #[test]
    fn comma_in_middleware_reference_is_rejected() {
        let mut fields = expose_with_host("ok.example.com");
        fields.middleware = refs(&["a,b"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "middleware",
                character: ',',
                ..
            }
        ));
    }

    /// A hostname with the punctuation real hostnames actually use
    /// stays accepted — the guard is a short deny-list, not a hostname
    /// grammar.
    #[test]
    fn ordinary_hostname_punctuation_is_still_accepted() {
        let fields = expose_with_host("my-service.sub_domain.example.com:8443");
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.s.rule=Host(`my-service.sub_domain.example.com:8443`)"]
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
                entrypoint: refs(&["web-secure"]),
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

    // --- traefik { disabled } (#159) ---

    fn disabled_traefik() -> Traefik {
        Traefik {
            disabled: Some(span()),
            span: span(),
        }
    }

    /// The baseline case: a disabled service with nothing else set emits
    /// exactly `traefik.enable=false`, no `traefik.docker.network=`
    /// included even when one would otherwise apply — see [`compute`]'s
    /// own doc for why that label is left out too.
    #[test]
    fn disabled_service_emits_only_enable_false() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            ..Default::default()
        };
        let labels = compute("db", &fields, Some("docker_default"), &bindings()).unwrap();
        assert_eq!(labels, vec!["traefik.enable=false"]);
    }

    /// `expose.port` is Compose's own `expose:` key, not Traefik's — a
    /// disabled service may still declare it, and doing so changes
    /// nothing about the Traefik label list.
    #[test]
    fn disabled_service_with_only_expose_port_still_emits_only_enable_false() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            expose: Some(Expose {
                port: Some(Literal::Number {
                    text: "5432".to_string(),
                    value: 5432,
                    span: span(),
                }),
                host: None,
                entrypoint: Vec::new(),
                span: span(),
            }),
            ..Default::default()
        };
        let labels = compute("db", &fields, None, &bindings()).unwrap();
        assert_eq!(labels, vec!["traefik.enable=false"]);
    }

    /// `expose.host` on a disabled service is the direct contradiction
    /// the issue calls out: reported before `expose.entrypoint` and
    /// `middleware`, since it's the field that would have created the
    /// router those two attach to.
    #[test]
    fn disabled_service_with_expose_host_is_rejected() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            expose: Some(Expose {
                port: None,
                host: Some(lit("db.example.com")),
                entrypoint: Vec::new(),
                span: span(),
            }),
            ..Default::default()
        };
        let err = compute("db", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "expose.host",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn disabled_service_with_entrypoint_is_rejected() {
        let mut fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            ..Default::default()
        };
        fields.expose = Some(Expose {
            port: None,
            host: None,
            entrypoint: refs(&["web-secure"]),
            span: span(),
        });
        let err = compute("db", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "expose.entrypoint",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn disabled_service_with_middleware_is_rejected() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            middleware: refs(&["forwardAuth-authentik"]),
            ..Default::default()
        };
        let err = compute("db", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "middleware",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// When a disabled service contradicts itself on more than one
    /// front, `expose.host` is the one named — the same "first in
    /// emission order" convention `entrypoint_without_a_host_is_rejected_before_middleware`
    /// already documents for the router-less case.
    #[test]
    fn disabled_service_with_host_and_middleware_reports_host_first() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            expose: Some(Expose {
                port: None,
                host: Some(lit("db.example.com")),
                entrypoint: Vec::new(),
                span: span(),
            }),
            middleware: refs(&["forwardAuth-authentik"]),
            ..Default::default()
        };
        let err = compute("db", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "expose.host",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A service that never touches `traefik` at all keeps every
    /// existing behavior byte for byte — the guard clause only fires
    /// when `fields.traefik.disabled` is actually `Some`.
    #[test]
    fn no_traefik_field_leaves_ordinary_computation_untouched() {
        let fields = expose_with_host("syncthing.internal.techdebtor.io");
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)"]
        );
    }
}
