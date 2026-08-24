//! Computes one service's Traefik labels. These live on the service's
//! own `labels:` list, not a separate config file — Traefik's Docker
//! provider reads labels off container metadata directly, confirmed
//! against every real Docker-based service in the homelab this targets.

use std::collections::HashMap;

use hl_parser::{Reference, Router, ServiceFields, Span};

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

/// Whether `c` may appear in a `router` block's name (#184).
///
/// The name is spliced into a label *key* —
/// `traefik.http.routers.<name>.rule` — not into a value, so
/// [`LABEL_METACHARACTERS`] is the wrong set here: it's tuned for the
/// rule grammar a *value* lands in, and it lets through both `.` and
/// `=`, the two characters that matter most on this side. A `.` extends
/// the dotted key (`traefik.http.routers.a.b.rule`), and an `=` ends it
/// outright — Docker splits a label string on its first `=`, so a router
/// named `x=y` writes the key `traefik.http.routers.x` with the value
/// `y.rule=...`, which is a forged label, not a corrupted one.
///
/// So this is an allow-list rather than a deny-list: exactly what a
/// Traefik router name may hold, and exactly what an `IDENT` — the only
/// spelling the grammar accepts for a router name — can already contain.
/// The two locks are deliberate. The grammar makes a dangerous name
/// unwritable, and this makes it unemittable, so codegen's safety
/// doesn't rest on the parser's grammar staying the way it is.
fn is_router_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Rejects a router name holding anything [`is_router_name_char`]
/// doesn't allow.
fn reject_unsafe_router_name(
    service_name: &str,
    name: &str,
    span: Span,
) -> Result<(), CodegenError> {
    match name.chars().find(|c| !is_router_name_char(*c)) {
        Some(character) => Err(CodegenError::UnsafeRouterName {
            service: service_name.to_string(),
            name: name.to_string(),
            character,
            span,
        }),
        None => Ok(()),
    }
}

/// Rejects `value` if it contains any of `forbidden`, or any control
/// character. Always applied to the *resolved* value, after `{{}}`
/// interpolation, since that's the text that actually reaches the label.
///
/// The control-character half is what keeps the sets above honest now
/// that a string literal has escape sequences (#181). A newline used to
/// be unrepresentable in `.hll` source, so `host "a\nb"` could only ever
/// be the two characters `\` and `n` — and `\` is already forbidden.
/// Written as `\n` it's now one real newline, which no list of shell and
/// rule metacharacters would have caught, and which cannot appear in a
/// hostname or an entry point name for any legitimate reason. Testing
/// the character class rather than adding three more entries to
/// [`LABEL_METACHARACTERS`] also covers a control character typed
/// directly into a literal, which was always possible for every one of
/// them except the newline.
fn reject_metacharacters(
    value: &str,
    field: &'static str,
    forbidden: &[char],
    span: Span,
) -> Result<(), CodegenError> {
    match value
        .chars()
        .find(|c| forbidden.contains(c) || c.is_control())
    {
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
///
/// The two fields need different routers, which is why `router` blocks
/// (#184) only rescue one of them. `expose.entrypoint`'s label goes on
/// `traefik.http.routers.<service>` specifically — the router
/// `expose.host` and nothing else creates — so a `router` block can't
/// stand in for the missing host there. `middleware` is service-level
/// and attaches to *every* router the service has, so a service with a
/// `router` block has somewhere to put it and isn't router-less at all.
/// A service with no `router` blocks reaches exactly the same answer
/// this function gave before they existed.
fn router_field_needing_host(fields: &ServiceFields) -> Option<(&'static str, Span)> {
    fields
        .expose
        .as_ref()
        .and_then(|e| e.entrypoint.first())
        .map(|r| ("expose.entrypoint", r.name_span))
        .or_else(|| {
            if !fields.routers.is_empty() {
                return None;
            }
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
/// `expose.host` leads because disabling Traefik makes even the host
/// that would have *created* the router a contradiction, not just the
/// fields that would attach to it. A `router` block (#184) is the same
/// kind of contradiction one step further along — a whole router
/// declared on a service that just said it wants none — and sorts
/// between the `expose` sub-fields and `middleware`, which is where its
/// labels would have been emitted.
///
/// This deliberately spells the order out rather than reusing
/// [`router_field_needing_host`] for its last two rows, as it used to:
/// that function now answers a narrower question (which field lacks a
/// router it could attach to), and a `router` block makes the two
/// questions diverge — it *gives* `middleware` a router, so
/// `router_field_needing_host` stops reporting `middleware`, while a
/// disabled service contradicts both.
fn traefik_conflict_field(fields: &ServiceFields) -> Option<(&'static str, Span)> {
    fields
        .expose
        .as_ref()
        .and_then(|e| e.host.as_ref())
        .map(|host| ("expose.host", host.span()))
        .or_else(|| {
            fields
                .expose
                .as_ref()
                .and_then(|e| e.entrypoint.first())
                .map(|r| ("expose.entrypoint", r.name_span))
        })
        .or_else(|| fields.routers.first().map(|r| ("router", r.span)))
        .or_else(|| {
            fields
                .middleware
                .first()
                .map(|r| ("middleware", r.name_span))
        })
}

/// The router id a `router` block's labels are keyed under: the
/// service's own name for the unnamed form — the very id `expose.host`
/// produces, which is why [`compute`] refuses both at once — and
/// `<service>-<name>` for a named one.
fn router_id(service_name: &str, router: &Router) -> String {
    match router.key() {
        Some(name) => format!("{service_name}-{name}"),
        None => service_name.to_string(),
    }
}

/// The ``Host(`h`)`` rule, with `path_prefix`'s alternatives `&&`-ed on
/// as one parenthesized `||` group when there are any (#184).
///
/// The parentheses are not optional and not cosmetic: `&&` binds tighter
/// than `||` in Traefik's rule grammar, so ``Host(`h`) && PathPrefix(`a`)
/// || PathPrefix(`b`)`` matches *any* host under `/b`. They're emitted
/// for a single prefix too, where they change nothing, because a rule
/// whose shape depends on how many prefixes it happens to have is a rule
/// nobody can read off the source.
fn rule_value(host: &str, path_prefixes: &[String]) -> String {
    if path_prefixes.is_empty() {
        return format!("Host(`{host}`)");
    }
    let alternatives: Vec<String> = path_prefixes
        .iter()
        .map(|p| format!("PathPrefix(`{p}`)"))
        .collect();
    format!("Host(`{host}`) && ({})", alternatives.join(" || "))
}

/// The one `entrypoints=` label for `entrypoint`, or `None` when the
/// list is empty — Traefik's own way of saying "attach to every entry
/// point," rather than a homelab-specific default the compiler picks.
///
/// Interpolation runs per entry, before validation, for the reason it
/// always has: `{{name}}` is resolved here, so the resolved text is what
/// actually reaches the label and therefore what the metacharacter guard
/// has to inspect. An entry point spelled as a string (`entrypoint
/// "{{name}}-secure"`) is the case that makes this observable.
fn entrypoints_label(
    id: &str,
    field: &'static str,
    entrypoint: &[Reference],
    bindings: &HashMap<&str, &str>,
) -> Result<Option<String>, CodegenError> {
    if entrypoint.is_empty() {
        return Ok(None);
    }
    let mut eps = Vec::with_capacity(entrypoint.len());
    for r in entrypoint {
        let resolved = interp::resolve(&r.name, bindings, r.name_span)?;
        reject_metacharacters(&resolved, field, LABEL_METACHARACTERS, r.name_span)?;
        eps.push(resolved);
    }
    Ok(Some(format!(
        "traefik.http.routers.{id}.entrypoints={}",
        eps.join(",")
    )))
}

/// The one `middlewares=` label for `id`, or `None` when the service
/// names no middleware.
///
/// `middleware` is a service-level field and stays one (#184): it
/// attaches to every router the service has, so a service with three
/// `router` blocks and a `middleware` gets the same list on all three.
/// Per-router middleware isn't expressible yet.
///
/// Called once per router rather than resolved once up front, so the
/// first metacharacter rejection a service hits is still the first one
/// in *emission* order — the convention every other diagnostic here
/// follows. The work is a handful of string comparisons and the result
/// is identical each time.
fn middlewares_label(id: &str, middleware: &[Reference]) -> Result<Option<String>, CodegenError> {
    if middleware.is_empty() {
        return Ok(None);
    }
    let mut mws = Vec::with_capacity(middleware.len());
    for r in middleware {
        reject_metacharacters(
            &r.name,
            "middleware",
            MIDDLEWARE_METACHARACTERS,
            r.name_span,
        )?;
        mws.push(format!("{}@file", r.name));
    }
    Ok(Some(format!(
        "traefik.http.routers.{id}.middlewares={}",
        mws.join(",")
    )))
}

/// Emits one `router` block's labels — rule, then `entrypoints=`, then
/// `middlewares=` — in the same order and the same shape `expose.host`'s
/// own router emits them (#184).
fn push_router_labels(
    labels: &mut Vec<String>,
    service_name: &str,
    router: &Router,
    middleware: &[Reference],
    bindings: &HashMap<&str, &str>,
) -> Result<(), CodegenError> {
    if let Some(name) = router.key() {
        reject_unsafe_router_name(service_name, name, router.span)?;
    }
    let id = router_id(service_name, router);

    let host_lit = match &router.host {
        Some(host) => host,
        None => {
            return Err(CodegenError::RouterBlockWithoutHost {
                service: service_name.to_string(),
                router: router.key().map(str::to_string),
                span: router.span,
            });
        }
    };
    let host = interp::resolve(host_lit.text(), bindings, host_lit.span())?;
    reject_metacharacters(&host, "router.host", LABEL_METACHARACTERS, host_lit.span())?;

    let mut prefixes = Vec::with_capacity(router.path_prefix.len());
    for prefix in &router.path_prefix {
        let resolved = interp::resolve(prefix.text(), bindings, prefix.span())?;
        reject_metacharacters(
            &resolved,
            "router.path_prefix",
            LABEL_METACHARACTERS,
            prefix.span(),
        )?;
        prefixes.push(resolved);
    }

    labels.push(format!(
        "traefik.http.routers.{id}.rule={}",
        rule_value(&host, &prefixes)
    ));
    if let Some(label) = entrypoints_label(&id, "router.entrypoint", &router.entrypoint, bindings)?
    {
        labels.push(label);
    }
    if let Some(label) = middlewares_label(&id, middleware)? {
        labels.push(label);
    }
    Ok(())
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
/// the loadbalancer port (if `expose.port` is set and the service has a
/// router at all) — emitted whenever a port is set, even when
/// technically redundant with Traefik's single-port default, matching
/// every real example's own "always explicit" convention.
///
/// Then, after that, one such group per `router` block (#184), in source
/// order: `<service>-<name>.rule=` (with `path_prefix`'s alternatives
/// `&&`-ed onto the `Host()` when the block sets any), its own
/// `.entrypoints=`, and the same service-level `.middlewares=` list the
/// `expose` router gets. A service that writes no `router` block reaches
/// none of that and emits exactly the list it always did, byte for byte
/// — the whole reason `router` is its own repeatable field rather than a
/// change to `expose`.
///
/// Both router-attached fields — `expose.entrypoint` and `middleware` —
/// require a router to attach to. `expose.entrypoint` requires
/// `expose.host` specifically, since its label goes on the router only
/// that host creates; `middleware` is satisfied by any router the
/// service has, `expose.host`'s or a `router` block's. Setting either
/// with nothing to attach to is [`CodegenError::RouterFieldWithoutHost`],
/// not a silently label-less service (#80). A `router` block that sets
/// no `host` at all is [`CodegenError::RouterBlockWithoutHost`], for the
/// same reason one step in, and a service that sets both `expose.host`
/// and an *unnamed* `router` is
/// [`CodegenError::ExposeHostWithUnnamedRouter`], since the two would
/// claim one router id.
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

    let expose_host = fields.expose.as_ref().and_then(|e| e.host.as_ref());

    // Checked before any label is emitted, so "no `expose` block at all"
    // and "an `expose` block with no `host`" take the same path — a
    // `middleware` line is equally router-less either way.
    if expose_host.is_none()
        && let Some((field, span)) = router_field_needing_host(fields)
    {
        return Err(CodegenError::RouterFieldWithoutHost {
            service: service_name.to_string(),
            field,
            span,
        });
    }

    // An unnamed `router { }` claims `traefik.http.routers.<service>`,
    // which is precisely the id `expose.host` produces — two blocks
    // writing the same label keys, one silently overwriting the other in
    // the emitted list. Refused rather than resolved in either
    // direction, since neither reading is the obvious one (#184).
    if let Some(host_lit) = expose_host
        && let Some(unnamed) = fields.routers.iter().find(|r| r.name.is_none())
    {
        return Err(CodegenError::ExposeHostWithUnnamedRouter {
            service: service_name.to_string(),
            host_span: host_lit.span(),
            span: unnamed.span,
        });
    }

    if let Some(host_lit) = expose_host {
        let host = interp::resolve(host_lit.text(), bindings, host_lit.span())?;
        reject_metacharacters(&host, "expose.host", LABEL_METACHARACTERS, host_lit.span())?;
        labels.push(format!(
            "traefik.http.routers.{service_name}.rule=Host(`{host}`)"
        ));

        let entrypoint = fields
            .expose
            .as_ref()
            .map(|e| e.entrypoint.as_slice())
            .unwrap_or_default();
        if let Some(label) =
            entrypoints_label(service_name, "expose.entrypoint", entrypoint, bindings)?
        {
            labels.push(label);
        }
        if let Some(label) = middlewares_label(service_name, &fields.middleware)? {
            labels.push(label);
        }
    }

    // In source order (#184), which composition has already made a
    // stable function of the source — see `compose.rs`'s `merge_routers`.
    for router in &fields.routers {
        push_router_labels(
            &mut labels,
            service_name,
            router,
            &fields.middleware,
            bindings,
        )?;
    }

    // Per Compose *service*, not per router: a container listens on one
    // port however many routers point at it, so this stays exactly where
    // it was — derived from `expose.port`, emitted once, and only when
    // the service has a router for it to serve. A `router` block carries
    // no port of its own for the same reason (#184).
    if let Some(port) = fields.expose.as_ref().and_then(|e| e.port.as_ref())
        && (expose_host.is_some() || !fields.routers.is_empty())
    {
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
    use hl_parser::{Expose, Ident, Literal, Reference, Traefik};
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

    /// #181: `host "a\nb"` used to be the two characters `\` and `n`,
    /// caught by the `\` in [`LABEL_METACHARACTERS`]. Now that a string
    /// literal decodes escapes, it's one real newline — a character no
    /// hostname can contain, and one the metacharacter list alone
    /// wouldn't have stopped.
    #[test]
    fn newline_in_host_is_rejected() {
        let fields = expose_with_host("ok.example.com\nsecond.example.com");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.host",
                character: '\n',
                ..
            }
        ));
    }

    /// The same for an entry point name, which reaches a label the
    /// codegen joins with commas.
    #[test]
    fn newline_in_entrypoint_is_rejected() {
        let mut fields = expose_with_host("ok.example.com");
        fields.expose.as_mut().unwrap().entrypoint = refs(&["web\nsecure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.entrypoint",
                character: '\n',
                ..
            }
        ));
    }

    /// And for a middleware reference, whose own set is just the comma
    /// that joins them — the control-character check is what covers it.
    #[test]
    fn newline_in_middleware_reference_is_rejected() {
        let mut fields = expose_with_host("ok.example.com");
        fields.middleware = refs(&["authentik\nsecond"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "middleware",
                character: '\n',
                ..
            }
        ));
    }

    /// A tab is no more a hostname character than a newline is, and a
    /// literal one could always be typed straight into a string.
    #[test]
    fn tab_in_host_is_rejected() {
        let fields = expose_with_host("ok.example.com\t");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "expose.host",
                character: '\t',
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

    // --- `router` blocks (#184) ---

    fn router(name: Option<&str>, host: Option<&str>) -> Router {
        Router {
            name: name.map(|n| Ident {
                name: n.to_string(),
                span: span(),
            }),
            host: host.map(lit),
            entrypoint: Vec::new(),
            path_prefix: Vec::new(),
            span: span(),
        }
    }

    fn with_routers(routers: Vec<Router>) -> ServiceFields {
        ServiceFields {
            routers,
            ..Default::default()
        }
    }

    fn prefixes(paths: &[&str]) -> Vec<Literal> {
        paths.iter().map(|p| lit(p)).collect()
    }

    /// The whole of #184 in one assertion: the exact label set the issue
    /// asks for, from four `router` blocks off one container. Read it
    /// against the issue's own hand-written `raw { labels: [...] }`
    /// list — every router label matches, label for label.
    #[test]
    fn four_routers_produce_the_issue_s_own_label_set() {
        let mut api = router(Some("api"), Some("vikunja.techdebtor.io"));
        api.entrypoint = refs(&["web-secure"]);
        api.path_prefix = prefixes(&["/api/v1", "/dav/", "/.well-known/"]);
        let mut api_local = router(Some("api-local"), Some("vikunja.techdebtor.local"));
        api_local.entrypoint = refs(&["local"]);
        api_local.path_prefix = prefixes(&["/api/v1", "/dav/", "/.well-known/"]);
        let mut frontend = router(Some("frontend"), Some("vikunja.techdebtor.io"));
        frontend.entrypoint = refs(&["web-secure"]);
        let mut frontend_local = router(Some("frontend-local"), Some("vikunja.techdebtor.local"));
        frontend_local.entrypoint = refs(&["local"]);

        let fields = with_routers(vec![api, api_local, frontend, frontend_local]);
        let labels = compute("vikunja", &fields, Some("docker_default"), &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.docker.network=docker_default",
                "traefik.http.routers.vikunja-api.rule=Host(`vikunja.techdebtor.io`) && (PathPrefix(`/api/v1`) || PathPrefix(`/dav/`) || PathPrefix(`/.well-known/`))",
                "traefik.http.routers.vikunja-api.entrypoints=web-secure",
                "traefik.http.routers.vikunja-api-local.rule=Host(`vikunja.techdebtor.local`) && (PathPrefix(`/api/v1`) || PathPrefix(`/dav/`) || PathPrefix(`/.well-known/`))",
                "traefik.http.routers.vikunja-api-local.entrypoints=local",
                "traefik.http.routers.vikunja-frontend.rule=Host(`vikunja.techdebtor.io`)",
                "traefik.http.routers.vikunja-frontend.entrypoints=web-secure",
                "traefik.http.routers.vikunja-frontend-local.rule=Host(`vikunja.techdebtor.local`)",
                "traefik.http.routers.vikunja-frontend-local.entrypoints=local",
            ]
        );
    }

    /// A named router keys its labels under `<service>-<name>`, leaving
    /// the bare `<service>` id free for `expose.host`.
    #[test]
    fn named_router_keys_labels_under_service_dash_name() {
        let fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.app-api.rule=Host(`a.example.com`)"]
        );
    }

    /// The unnamed form claims the service's own id — exactly what
    /// `expose.host` produces, which is what makes writing both an
    /// error.
    #[test]
    fn unnamed_router_keys_labels_under_the_service_name() {
        let fields = with_routers(vec![router(None, Some("a.example.com"))]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.app.rule=Host(`a.example.com`)"]
        );
    }

    /// Several routers emit in source order, each complete before the
    /// next begins, so the label list reads top to bottom the way the
    /// source does.
    #[test]
    fn several_routers_emit_in_source_order() {
        let mut first = router(Some("one"), Some("one.example.com"));
        first.entrypoint = refs(&["web-secure"]);
        let second = router(Some("two"), Some("two.example.com"));
        let fields = with_routers(vec![first, second]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-one.rule=Host(`one.example.com`)",
                "traefik.http.routers.app-one.entrypoints=web-secure",
                "traefik.http.routers.app-two.rule=Host(`two.example.com`)",
            ]
        );
    }

    /// Three prefixes become one parenthesized `||` group `&&`-ed onto
    /// the host. The parentheses are load-bearing: `&&` binds tighter
    /// than `||`, so without them the rule would match any host under
    /// the last prefix.
    #[test]
    fn three_path_prefixes_join_into_one_parenthesized_group() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.path_prefix = prefixes(&["/api/v1", "/dav/", "/.well-known/"]);
        let fields = with_routers(vec![r]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`) && (PathPrefix(`/api/v1`) || PathPrefix(`/dav/`) || PathPrefix(`/.well-known/`))"
            ]
        );
    }

    /// A single prefix keeps the parentheses too, even though they
    /// change nothing there — one rule shape to read, not two.
    #[test]
    fn one_path_prefix_still_gets_its_parentheses() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.path_prefix = prefixes(&["/api/v1"]);
        let fields = with_routers(vec![r]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`) && (PathPrefix(`/api/v1`))"
            ]
        );
    }

    /// No prefixes leaves the rule exactly what `expose.host` has always
    /// produced.
    #[test]
    fn no_path_prefix_leaves_the_rule_a_bare_host_match() {
        let fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert!(
            labels[0].ends_with(".rule=Host(`a.example.com`)"),
            "{labels:?}"
        );
    }

    /// A `router` block with no `host` has no rule to emit and can't
    /// have meant anything — the mirror of `expose`'s own
    /// host-less-router rule, one level in.
    #[test]
    fn router_without_a_host_is_rejected() {
        let fields = with_routers(vec![router(Some("api"), None)]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterBlockWithoutHost { ref router, .. } if router.as_deref() == Some("api")
            ),
            "got {err:?}"
        );
    }

    /// Including the unnamed form, which reports `None` rather than a
    /// name it doesn't have.
    #[test]
    fn unnamed_router_without_a_host_is_rejected() {
        let fields = with_routers(vec![router(None, None)]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterBlockWithoutHost { router: None, .. }
            ),
            "got {err:?}"
        );
    }

    /// An entry point with no host is caught by the same rule: the block
    /// still has no `host`, whatever else it sets.
    #[test]
    fn router_entrypoint_without_a_host_is_rejected() {
        let mut r = router(Some("api"), None);
        r.entrypoint = refs(&["web-secure"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterBlockWithoutHost { .. }),
            "got {err:?}"
        );
    }

    /// As is a `path_prefix` with no host — a path to match under no
    /// host to match it on.
    #[test]
    fn router_path_prefix_without_a_host_is_rejected() {
        let mut r = router(Some("api"), None);
        r.path_prefix = prefixes(&["/api/v1"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterBlockWithoutHost { .. }),
            "got {err:?}"
        );
    }

    /// `expose.host` and an unnamed `router` both claim
    /// `traefik.http.routers.<service>`, so one would silently overwrite
    /// the other in the emitted list. Refused rather than resolved.
    #[test]
    fn expose_host_plus_an_unnamed_router_is_rejected() {
        let mut fields = expose_with_host("a.example.com");
        fields.routers = vec![router(None, Some("b.example.com"))];
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::ExposeHostWithUnnamedRouter { .. }),
            "got {err:?}"
        );
    }

    /// A *named* router alongside `expose.host` is the ordinary case,
    /// not a collision: the two ids differ, so both routers are emitted,
    /// `expose`'s first.
    #[test]
    fn expose_host_plus_a_named_router_emits_both() {
        let mut fields = expose_with_host("a.example.com");
        fields.routers = vec![router(Some("api"), Some("b.example.com"))];
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.rule=Host(`b.example.com`)",
            ]
        );
    }

    /// A router name lands in a label *key*, so a `.` in one doesn't
    /// corrupt a value — it forges a different label. `traefik.http.
    /// routers.app-a.tls.certresolver` is a real Traefik key, so a
    /// router named `a.tls.certresolver` would write it with whatever
    /// value the rule happened to hold. The grammar can't spell such a
    /// name (a router name is an `IDENT`), and this is the second lock
    /// on that same door.
    #[test]
    fn dot_in_a_router_name_is_rejected() {
        let fields = with_routers(vec![router(
            Some("a.tls.certresolver"),
            Some("a.example.com"),
        )]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::UnsafeRouterName { character: '.', .. }),
            "got {err:?}"
        );
    }

    /// The other half of the same hole: Docker splits a label string on
    /// its first `=`, so a router named `x=y` writes the *key*
    /// `traefik.http.routers.app-x` with the value `y.rule=...` — an
    /// entirely different label than the one the source asked for.
    #[test]
    fn equals_in_a_router_name_is_rejected() {
        let fields = with_routers(vec![router(Some("x=y"), Some("a.example.com"))]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::UnsafeRouterName { character: '=', .. }),
            "got {err:?}"
        );
    }

    /// Whitespace and backticks are refused for the same reason, and
    /// against the same allow-list — the check is "what may a Traefik
    /// router name hold," not a list of characters someone thought of.
    #[test]
    fn whitespace_and_backtick_in_a_router_name_are_rejected() {
        for name in ["a b", "a`b"] {
            let fields = with_routers(vec![router(Some(name), Some("a.example.com"))]);
            let err = compute("app", &fields, None, &bindings()).unwrap_err();
            assert!(
                matches!(err, CodegenError::UnsafeRouterName { .. }),
                "expected {name:?} to be rejected, got {err:?}"
            );
        }
    }

    /// The names real routers actually use stay accepted: letters,
    /// digits, `-`, `_`.
    #[test]
    fn ordinary_router_names_are_accepted() {
        for name in ["api", "api-local", "api_v1", "v2"] {
            let fields = with_routers(vec![router(Some(name), Some("a.example.com"))]);
            let labels = compute("app", &fields, None, &bindings()).unwrap();
            assert_eq!(
                labels,
                vec![format!(
                    "traefik.http.routers.app-{name}.rule=Host(`a.example.com`)"
                )]
            );
        }
    }

    /// The value-side guard covers a `router`'s own `host` exactly as it
    /// covers `expose.host` — the same rule grammar, the same backtick
    /// escape from #65.
    #[test]
    fn backtick_in_a_router_host_is_rejected() {
        let fields = with_routers(vec![router(
            Some("api"),
            Some("ok.example.com`) || HostRegexp(`{any:.+}"),
        )]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.host",
                    character: '`',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// And each `path_prefix`, which is spliced into the same rule
    /// inside its own ``PathPrefix(`...`)``.
    #[test]
    fn backtick_in_a_path_prefix_is_rejected() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.path_prefix = prefixes(&["/api`) || PathPrefix(`/"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.path_prefix",
                    character: '`',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// And each entry point, comma included, since codegen owns the
    /// comma that joins them.
    #[test]
    fn comma_in_a_router_entrypoint_is_rejected() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.entrypoint = refs(&["web,web-secure"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.entrypoint",
                    character: ',',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// Every value guard here runs on the *resolved* text, so a
    /// metacharacter that only appears after `{{name}}` interpolation is
    /// still caught — `expose.host`'s own rule, applied to `router`.
    #[test]
    fn router_host_is_checked_after_interpolation() {
        let fields = with_routers(vec![router(Some("api"), Some("{{name}}.example.com"))]);
        let bindings = HashMap::from([("name", "bad`)")]);
        let err = compute("app", &fields, None, &bindings).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.host",
                    character: '`',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// And the resolved text, not the raw `{{name}}`, is what lands in
    /// the rule — the `"{{name}}.internal.example.com"` template idiom
    /// works in a `router` exactly as it does in `expose`.
    #[test]
    fn router_host_is_interpolated_into_the_rule() {
        let fields = with_routers(vec![router(Some("api"), Some("{{name}}.example.com"))]);
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec!["traefik.http.routers.syncthing-api.rule=Host(`syncthing.example.com`)"]
        );
    }

    /// `middleware` stays a service-level field: one list, attached to
    /// every router the service has, `expose`'s included. Per-router
    /// middleware isn't expressible yet.
    #[test]
    fn middleware_attaches_to_every_router() {
        let mut fields = expose_with_host("a.example.com");
        fields.middleware = refs(&["forwardAuth-authentik"]);
        fields.routers = vec![router(Some("api"), Some("b.example.com"))];
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app.rule=Host(`a.example.com`)",
                "traefik.http.routers.app.middlewares=forwardAuth-authentik@file",
                "traefik.http.routers.app-api.rule=Host(`b.example.com`)",
                "traefik.http.routers.app-api.middlewares=forwardAuth-authentik@file",
            ]
        );
    }

    /// A `router` block is a router, so `middleware` beside one has
    /// somewhere to attach and is no longer the router-less mistake #80
    /// refuses — even with no `expose.host` anywhere.
    #[test]
    fn middleware_with_a_router_and_no_expose_host_is_accepted() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.middleware = refs(&["forwardAuth-authentik"]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.middlewares=forwardAuth-authentik@file",
            ]
        );
    }

    /// `expose.entrypoint` is *not* rescued by a `router` block: its
    /// label goes on `traefik.http.routers.<service>` specifically, the
    /// router only `expose.host` creates.
    #[test]
    fn expose_entrypoint_still_needs_expose_host_even_beside_a_router() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.expose = Some(Expose {
            port: None,
            host: None,
            entrypoint: refs(&["web-secure"]),
            span: span(),
        });
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterFieldWithoutHost {
                    field: "expose.entrypoint",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// `loadbalancer.server.port` stays per Compose service, derived
    /// from `expose.port`: one label however many routers point at the
    /// container, and emitted for a service whose only routers are
    /// `router` blocks.
    #[test]
    fn expose_port_still_produces_exactly_one_loadbalancer_label() {
        let mut fields = with_routers(vec![
            router(Some("api"), Some("a.example.com")),
            router(Some("web"), Some("b.example.com")),
        ]);
        fields.expose = Some(Expose {
            port: Some(Literal::Number {
                text: "3456".to_string(),
                value: 3456,
                span: span(),
            }),
            host: None,
            entrypoint: Vec::new(),
            span: span(),
        });
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels
                .iter()
                .filter(|l| l.contains("loadbalancer.server.port"))
                .collect::<Vec<_>>(),
            vec!["traefik.http.services.app.loadbalancer.server.port=3456"]
        );
        // And still last, after every router's own labels — the
        // service-wide statement that closes the list.
        assert!(
            labels
                .last()
                .is_some_and(|l| l.contains("loadbalancer.server.port")),
            "{labels:?}"
        );
    }

    /// `traefik { disabled }` covers `router` blocks too: a whole router
    /// declared on a service that just said it wants none is the same
    /// contradiction `expose.host` raises there.
    #[test]
    fn disabled_service_with_a_router_is_rejected() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.traefik = Some(disabled_traefik());
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "router",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// And a host-less one, which never gets as far as its own missing
    /// host — the flag contradicts the block's existence, not its
    /// contents.
    #[test]
    fn disabled_service_with_a_hostless_router_is_rejected_as_a_conflict() {
        let mut fields = with_routers(vec![router(Some("api"), None)]);
        fields.traefik = Some(disabled_traefik());
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TraefikDisabledWithRouterField {
                    field: "router",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// The regression that matters most (#184's whole premise): a
    /// service that uses only `expose` reaches none of the new code and
    /// emits exactly what it emitted before `router` existed.
    #[test]
    fn an_expose_only_service_emits_exactly_what_it_always_did() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: Some(Literal::Number {
                    text: "8384".to_string(),
                    value: 8384,
                    span: span(),
                }),
                host: Some(lit("{{name}}.internal.techdebtor.io")),
                entrypoint: refs(&["web-secure"]),
                span: span(),
            }),
            middleware: refs(&["local-ipwhitelist", "forwardAuth-authentik"]),
            ..Default::default()
        };
        assert!(fields.routers.is_empty());
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
