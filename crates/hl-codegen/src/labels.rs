//! Computes one service's Traefik labels. These live on the service's
//! own `labels:` list, not a separate config file — Traefik's Docker
//! provider reads labels off container metadata directly, confirmed
//! against every real Docker-based service in the homelab this targets.

use std::collections::HashMap;

use hl_parser::{Literal, Router, ServiceFields, Span};

use crate::{CodegenError, interp};

/// Characters rejected in every label value the user writes directly —
/// a router's `host` and each `entrypoint` entry. Motivated by `host`,
/// which is spliced verbatim into
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
/// `entrypoint` shares this exact set, `,` included. It used to
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

/// The first `router` block a *disabled* service also declares, if it
/// declares one at all (#159) — the whole of what can contradict
/// `traefik { disabled }` since #221 folded `middleware` inside
/// `router`, where it can't be written without one.
///
/// `expose.port` deliberately isn't checked here: it's Compose's own
/// `expose:` key, plain container-network visibility with nothing to do
/// with Traefik, so a disabled service may still declare it (#159) —
/// see [`crate::CodegenError::TraefikDisabledWithRouter`]'s doc.
fn traefik_conflict_router(fields: &ServiceFields) -> Option<Span> {
    fields.routers.first().map(|r| r.span)
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
    entrypoint: &[Literal],
    bindings: &HashMap<&str, &str>,
) -> Result<Option<String>, CodegenError> {
    if entrypoint.is_empty() {
        return Ok(None);
    }
    let mut eps = Vec::with_capacity(entrypoint.len());
    for r in entrypoint {
        let resolved = interp::resolve(r.text(), bindings, r.span())?;
        reject_metacharacters(&resolved, field, LABEL_METACHARACTERS, r.span())?;
        eps.push(resolved);
    }
    Ok(Some(format!(
        "traefik.http.routers.{id}.entrypoints={}",
        eps.join(",")
    )))
}

/// The one `middlewares=` label for `id`, or `None` when the router
/// attaches no middleware.
///
/// `middleware` is a `router` field and only ever a `router` field
/// (#221): a middleware reaches Traefik as a label on one specific
/// router, so a service-wide list could not express two routers off one
/// container needing different ones — see
/// [`hl_parser::ast::Router::middleware`].
///
/// Each name gets an `@file` suffix, the file provider's own reference
/// convention, applied unconditionally — unlike `entrypoints=` above,
/// whose names carry no such suffix.
fn middlewares_label(id: &str, middleware: &[Literal]) -> Result<Option<String>, CodegenError> {
    if middleware.is_empty() {
        return Ok(None);
    }
    let mut mws = Vec::with_capacity(middleware.len());
    for r in middleware {
        reject_metacharacters(
            r.text(),
            "router.middleware",
            MIDDLEWARE_METACHARACTERS,
            r.span(),
        )?;
        mws.push(format!("{}@file", r.text()));
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
    bindings: &HashMap<&str, &str>,
) -> Result<(), CodegenError> {
    if let Some(name) = router.key() {
        reject_unsafe_router_name(service_name, name, router.span)?;
    }
    let id = router_id(service_name, router);

    let host_lit = match &router.host {
        Some(host) => host,
        None => {
            return Err(CodegenError::RouterWithoutHost {
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
    if let Some(label) = middlewares_label(&id, &router.middleware)? {
        labels.push(label);
    }
    Ok(())
}

/// Computes `service_name`'s Traefik label list, in this order:
/// `traefik.docker.network=` (if `docker_network` is set — the real name
/// of whichever of the service's declared networks is `external`), then
/// one group per `router` block (#184, #198), in source order —
/// `<id>.rule=` (with `path_prefix`'s alternatives `&&`-ed onto the
/// `Host()` when the block sets any, `<id>` being `<service>` for the
/// unnamed form and `<service>-<name>` for a named one), its own
/// `.entrypoints=` (if the block's `entrypoint` list is non-empty — one
/// comma-joined label for the whole list, the same shape as
/// `.middlewares=` below but with no `@file` suffix, which is a
/// file-provider convention specific to middleware references), and its
/// `.middlewares=` (if any, each getting an `@file` suffix — the file
/// provider's own reference convention, confirmed mechanical/always-on,
/// not homelab-specific — built from the block's own `middleware` list
/// when it named one and from the service-level `middleware` otherwise,
/// see [`effective_middleware`]) — and finally, once, the
/// loadbalancer port (from `expose.port`) if the service has any router
/// at all. A service that writes no `router` block (`expose <port> as
/// "<host>"`'s own sugared one included) reaches none of that and emits
/// only the `traefik.docker.network=` line, if any.
///
/// A `router` block that sets no `host` is
/// [`CodegenError::RouterWithoutHost`], not a silently label-less
/// service — the block that exists only to *be* a router says nothing
/// about which requests reach it. #80's companion check, for a
/// `middleware` with no router to attach it to, is gone with the
/// service-level field itself (#221): a router's own `middleware`
/// can't be written without the block it lives in, so the mistake is
/// no longer expressible.
/// Once every router block is confirmed to have a host, a service that
/// has at least one but sets no `expose <port>` is
/// [`CodegenError::RouterWithoutPort`] (#198): a router with no port to
/// load-balance onto used to mean Traefik silently guessed one, and now
/// means `hllc` refuses to compile instead.
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
        if let Some(span) = traefik_conflict_router(fields) {
            return Err(CodegenError::TraefikDisabledWithRouter {
                service: service_name.to_string(),
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

    // In source order (#184), which composition has already made a
    // stable function of the source — see `compose.rs`'s `merge_routers`.
    // `expose <port> as "<host>"`'s own sugared unnamed router is just
    // another entry here (`crate::parser::Parser::parse_expose_as_sugar`
    // pushes it during parsing), so it reaches the exact same
    // `push_router_labels` call every hand-written `router` block does.
    for router in &fields.routers {
        push_router_labels(&mut labels, service_name, router, bindings)?;
    }

    // Per Compose *service*, not per router: a container listens on one
    // port however many routers point at it, so this is emitted once,
    // only when the service has a router for it to serve — and, since
    // #198 left `expose.port` the one remaining source of a port, a
    // router with none at all is refused rather than left for Traefik to
    // guess (the live defect #198 closes: a service routed only by
    // `router` blocks with no `expose` used to emit no
    // `loadbalancer.server.port` label at all, silently).
    if !fields.routers.is_empty() {
        match fields.expose.as_ref().and_then(|e| e.port.as_ref()) {
            Some(port) => labels.push(format!(
                "traefik.http.services.{service_name}.loadbalancer.server.port={}",
                port.text()
            )),
            None => {
                return Err(CodegenError::RouterWithoutPort {
                    service: service_name.to_string(),
                    span: fields.routers[0].span,
                });
            }
        }
    }

    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_parser::{Expose, Ident, Literal, Traefik};
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

    fn refs(names: &[&str]) -> Vec<Literal> {
        names
            .iter()
            .map(|n| Literal::Ident((*n).to_string(), span()))
            .collect()
    }

    /// `expose { port }` (#198) — the one field left on `Expose`.
    fn expose_port(value: u64) -> Expose {
        Expose {
            port: Some(Literal::Number {
                text: value.to_string(),
                value,
                span: span(),
            }),
            span: span(),
        }
    }

    fn router(name: Option<&str>, host: Option<&str>) -> Router {
        Router {
            name: name.map(|n| Ident {
                name: n.to_string(),
                span: span(),
            }),
            host: host.map(lit),
            entrypoint: Vec::new(),
            path_prefix: Vec::new(),
            middleware: Vec::new(),
            span: span(),
        }
    }

    fn with_routers(routers: Vec<Router>) -> ServiceFields {
        ServiceFields {
            routers,
            ..Default::default()
        }
    }

    /// An unnamed `router { host: ... }` — the shape `expose <port> as
    /// "<host>"` desugars to (#198), and the direct replacement for the
    /// old `expose_with_host` helper this module used before `host` moved
    /// off `Expose`.
    fn router_with_host(host: &str) -> ServiceFields {
        with_routers(vec![router(None, Some(host))])
    }

    fn prefixes(paths: &[&str]) -> Vec<Literal> {
        paths.iter().map(|p| lit(p)).collect()
    }

    #[test]
    fn no_expose_means_no_router_labels() {
        let fields = ServiceFields::default();
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(labels.is_empty());
    }

    /// #80's router-less-`middleware` check is gone with the field it
    /// guarded (#221): a service with no `router` has nowhere to write a
    /// middleware at all, so the mistake it caught can't be built — the
    /// list lives on [`Router`], and a service holds only routers. What's
    /// left to pin is that a router-less service is still quietly legal,
    /// emitting its non-router labels and no diagnostic.
    #[test]
    fn a_service_with_no_router_has_nowhere_to_name_middleware() {
        let mut fields = ServiceFields::default();
        assert!(compute("s", &fields, None, &bindings()).unwrap().is_empty());

        fields.expose = Some(expose_port(80));
        assert!(compute("s", &fields, None, &bindings()).unwrap().is_empty());
    }

    /// A service with neither a router nor a port still generates its
    /// non-router labels and no diagnostic.
    #[test]
    fn an_expose_with_no_port_and_no_router_is_fine() {
        let fields = ServiceFields {
            expose: Some(Expose {
                port: None,
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
        let mut fields = router_with_host("syncthing.internal.techdebtor.io");
        fields.expose = Some(expose_port(8384));
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)",
                "traefik.http.services.syncthing.loadbalancer.server.port=8384",
            ]
        );
    }

    /// #65: the exact escape from the issue — a backtick closes the
    /// `Host()` value and the rest is read as more rule grammar,
    /// yielding a valid rule that matches every host.
    #[test]
    fn backtick_in_host_is_rejected() {
        let fields = router_with_host("ok.example.com`) || HostRegexp(`{any:.+}");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.host",
                character: '`',
                ..
            }
        ));
    }

    #[test]
    fn comma_in_host_is_rejected() {
        let fields = router_with_host("a.example.com,b.example.com");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.host",
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
        let fields = router_with_host("ok.example.com\nsecond.example.com");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.host",
                character: '\n',
                ..
            }
        ));
    }

    /// The same for an entry point name, which reaches a label the
    /// codegen joins with commas.
    #[test]
    fn newline_in_entrypoint_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["web\nsecure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoint",
                character: '\n',
                ..
            }
        ));
    }

    /// And for a middleware reference, whose own set is just the comma
    /// that joins them — the control-character check is what covers it.
    #[test]
    fn newline_in_middleware_reference_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].middleware = refs(&["authentik\nsecond"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.middleware",
                character: '\n',
                ..
            }
        ));
    }

    /// A tab is no more a hostname character than a newline is, and a
    /// literal one could always be typed straight into a string.
    #[test]
    fn tab_in_host_is_rejected() {
        let fields = router_with_host("ok.example.com\t");
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.host",
                character: '\t',
                ..
            }
        ));
    }

    /// The guard runs on the *resolved* value, so a metacharacter that
    /// only appears after `{{name}}` interpolation is still caught.
    #[test]
    fn host_is_checked_after_interpolation() {
        let fields = router_with_host("{{name}}.example.com");
        let bindings = HashMap::from([("name", "bad`)")]);
        let err = compute("s", &fields, None, &bindings).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.host",
                character: '`',
                ..
            }
        ));
    }

    #[test]
    fn backtick_in_entrypoint_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["web`-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoint",
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
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["web,web-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoint",
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
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["web", "web-secure"]);
        fields.expose = Some(expose_port(80));
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
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["web", "web-secure", "metrics"]);
        fields.expose = Some(expose_port(80));
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
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["{{name}}-secure"]);
        let bindings = HashMap::from([("name", "web`")]);
        let err = compute("s", &fields, None, &bindings).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoint",
                character: '`',
                ..
            }
        ));
    }

    /// And the resolved text — not the raw `{{name}}` source — is what
    /// lands in the label.
    #[test]
    fn entrypoint_is_interpolated_into_the_label() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoint = refs(&["{{name}}-secure"]);
        fields.expose = Some(expose_port(80));
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
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].middleware = refs(&["a,b"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.middleware",
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
        let mut fields = router_with_host("my-service.sub_domain.example.com:8443");
        fields.expose = Some(expose_port(80));
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.s.rule=Host(`my-service.sub_domain.example.com:8443`)",
                "traefik.http.services.s.loadbalancer.server.port=80",
            ]
        );
    }

    #[test]
    fn full_router_produces_all_labels_in_order() {
        let mut r = router(None, Some("syncthing.internal.techdebtor.io"));
        r.entrypoint = refs(&["web-secure"]);
        r.middleware = vec![
            Literal::Ident("local-ipwhitelist".to_string(), span()),
            Literal::Ident("forwardAuth-authentik".to_string(), span()),
        ];
        let fields = ServiceFields {
            routers: vec![r],
            expose: Some(expose_port(8384)),
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

    /// `expose <port>` is Compose's own `expose:` key, not Traefik's — a
    /// disabled service may still declare it, and doing so changes
    /// nothing about the Traefik label list.
    #[test]
    fn disabled_service_with_only_expose_port_still_emits_only_enable_false() {
        let fields = ServiceFields {
            traefik: Some(disabled_traefik()),
            expose: Some(expose_port(5432)),
            ..Default::default()
        };
        let labels = compute("db", &fields, None, &bindings()).unwrap();
        assert_eq!(labels, vec!["traefik.enable=false"]);
    }

    /// A middleware can only be written inside a `router` since #221,
    /// so a disabled service carrying one is refused for the block that
    /// holds it — there is no longer a second, field-shaped way to
    /// contradict `disabled`.
    #[test]
    fn disabled_service_with_router_middleware_is_rejected() {
        let mut r = router(Some("api"), Some("db.example.com"));
        r.middleware = refs(&["forwardAuth-authentik"]);
        let mut fields = with_routers(vec![r]);
        fields.traefik = Some(disabled_traefik());
        let err = compute("db", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::TraefikDisabledWithRouter { .. }),
            "got {err:?}"
        );
    }

    /// A service that never touches `traefik` at all keeps every
    /// existing behavior byte for byte — the guard clause only fires
    /// when `fields.traefik.disabled` is actually `Some`.
    #[test]
    fn no_traefik_field_leaves_ordinary_computation_untouched() {
        let mut fields = router_with_host("syncthing.internal.techdebtor.io");
        fields.expose = Some(expose_port(8384));
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.syncthing.rule=Host(`syncthing.internal.techdebtor.io`)",
                "traefik.http.services.syncthing.loadbalancer.server.port=8384",
            ]
        );
    }

    // --- `router` blocks (#184, #198) ---

    /// The whole of #184 in one assertion: the exact label set the issue
    /// asks for, from four `router` blocks off one container. Read it
    /// against the issue's own hand-written `raw { labels: [...] }`
    /// list — every router label matches, label for label. Also the
    /// `hl-cli` regression fixture `issue_184_multi_router.hll`'s own
    /// `expose 3456`, byte for byte.
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

        let mut fields = with_routers(vec![api, api_local, frontend, frontend_local]);
        fields.expose = Some(expose_port(3456));
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
                "traefik.http.services.vikunja.loadbalancer.server.port=3456",
            ]
        );
    }

    /// A named router keys its labels under `<service>-<name>`, leaving
    /// the bare `<service>` id free for the unnamed form.
    #[test]
    fn named_router_keys_labels_under_service_dash_name() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`)",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// The unnamed form claims the service's own id — which is what
    /// makes writing it twice in one body (by hand or via `expose <port>
    /// as "<host>"`'s own sugar) a `ParseError::DuplicateRouterName`.
    #[test]
    fn unnamed_router_keys_labels_under_the_service_name() {
        let mut fields = with_routers(vec![router(None, Some("a.example.com"))]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app.rule=Host(`a.example.com`)",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
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
        let mut fields = with_routers(vec![first, second]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-one.rule=Host(`one.example.com`)",
                "traefik.http.routers.app-one.entrypoints=web-secure",
                "traefik.http.routers.app-two.rule=Host(`two.example.com`)",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// Two routers sharing one port is the ordinary case, not a
    /// collision: distinct ids, one `loadbalancer.server.port=` label
    /// shared between them.
    #[test]
    fn unnamed_router_plus_a_named_router_emits_both() {
        let mut fields = with_routers(vec![
            router(None, Some("a.example.com")),
            router(Some("api"), Some("b.example.com")),
        ]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.rule=Host(`b.example.com`)",
                "traefik.http.services.app.loadbalancer.server.port=80",
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
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`) && (PathPrefix(`/api/v1`) || PathPrefix(`/dav/`) || PathPrefix(`/.well-known/`))",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// A single prefix keeps the parentheses too, even though they
    /// change nothing there — one rule shape to read, not two.
    #[test]
    fn one_path_prefix_still_gets_its_parentheses() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.path_prefix = prefixes(&["/api/v1"]);
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`) && (PathPrefix(`/api/v1`))",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// No prefixes leaves the rule a bare host match.
    #[test]
    fn no_path_prefix_leaves_the_rule_a_bare_host_match() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert!(
            labels[0].ends_with(".rule=Host(`a.example.com`)"),
            "{labels:?}"
        );
    }

    /// A `router` block with no `host` has no rule to emit and can't
    /// have meant anything.
    #[test]
    fn router_without_a_host_is_rejected() {
        let fields = with_routers(vec![router(Some("api"), None)]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterWithoutHost { ref router, .. } if router.as_deref() == Some("api")
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
            matches!(err, CodegenError::RouterWithoutHost { router: None, .. }),
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
            matches!(err, CodegenError::RouterWithoutHost { .. }),
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
            matches!(err, CodegenError::RouterWithoutHost { .. }),
            "got {err:?}"
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
            let mut fields = with_routers(vec![router(Some(name), Some("a.example.com"))]);
            fields.expose = Some(expose_port(80));
            let labels = compute("app", &fields, None, &bindings()).unwrap();
            assert_eq!(
                labels,
                vec![
                    format!("traefik.http.routers.app-{name}.rule=Host(`a.example.com`)"),
                    "traefik.http.services.app.loadbalancer.server.port=80".to_string(),
                ]
            );
        }
    }

    /// The value-side guard covers a `router`'s own `host` — the same
    /// rule grammar, the same backtick escape from #65.
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
    /// still caught.
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
    /// works in a `router` the same way it did in `expose.host` before
    /// #198.
    #[test]
    fn router_host_is_interpolated_into_the_rule() {
        let mut fields = with_routers(vec![router(Some("api"), Some("{{name}}.example.com"))]);
        fields.expose = Some(expose_port(80));
        let labels = compute("syncthing", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.syncthing-api.rule=Host(`syncthing.example.com`)",
                "traefik.http.services.syncthing.loadbalancer.server.port=80",
            ]
        );
    }

    /// The case the old service-level field existed for — every router
    /// wanting the same middleware — still works since #221, spelled by
    /// naming it on each router. That's more typing than one shared
    /// line, and deliberately so: the shared line couldn't express the
    /// routers that need to *differ*, which is the whole of #221.
    #[test]
    fn every_router_can_name_the_same_middleware() {
        let mut unnamed = router(None, Some("a.example.com"));
        unnamed.middleware = refs(&["forwardAuth-authentik"]);
        let mut api = router(Some("api"), Some("b.example.com"));
        api.middleware = refs(&["forwardAuth-authentik"]);
        let mut fields = with_routers(vec![unnamed, api]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app.rule=Host(`a.example.com`)",
                "traefik.http.routers.app.middlewares=forwardAuth-authentik@file",
                "traefik.http.routers.app-api.rule=Host(`b.example.com`)",
                "traefik.http.routers.app-api.middlewares=forwardAuth-authentik@file",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// `loadbalancer.server.port` stays per Compose service, derived
    /// from `expose.port`: one label however many routers point at the
    /// container.
    #[test]
    fn expose_port_still_produces_exactly_one_loadbalancer_label() {
        let mut fields = with_routers(vec![
            router(Some("api"), Some("a.example.com")),
            router(Some("web"), Some("b.example.com")),
        ]);
        fields.expose = Some(expose_port(3456));
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
    /// declared on a service that just said it wants none is a
    /// contradiction, port or no port.
    #[test]
    fn disabled_service_with_a_router_is_rejected() {
        let mut fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        fields.traefik = Some(disabled_traefik());
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::TraefikDisabledWithRouter { .. }),
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
            matches!(err, CodegenError::TraefikDisabledWithRouter { .. }),
            "got {err:?}"
        );
    }

    /// The regression that matters most (#198's whole premise): the
    /// exact scenario `expose <port> as "<host>"` desugars to — an
    /// unnamed router plus a port plus middleware — emits exactly the
    /// same label list `expose.host` always did, byte for byte, id
    /// included. See `hl-parser`'s own parser tests for proof the sugar
    /// itself still parses to this same shape.
    #[test]
    fn sugared_expose_as_router_emits_exactly_what_expose_host_always_did() {
        let mut r = router(None, Some("{{name}}.internal.techdebtor.io"));
        r.entrypoint = refs(&["web-secure"]);
        r.middleware = refs(&["local-ipwhitelist", "forwardAuth-authentik"]);
        let fields = ServiceFields {
            routers: vec![r],
            expose: Some(expose_port(8384)),
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

    // --- a router with no port to balance onto (#198) ---

    /// The live defect #198 closes: a service routed only by `router`
    /// blocks used to emit no `loadbalancer.server.port` label at all
    /// when it forgot `expose <port>` — valid output, Traefik left to
    /// guess. Now it's a compile error instead.
    #[test]
    fn router_without_a_port_is_rejected() {
        let fields = with_routers(vec![router(Some("api"), Some("a.example.com"))]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterWithoutPort { .. }),
            "got {err:?}"
        );
    }

    /// `expose <port>` with no router at all stays legal (constraint
    /// #3): it's Compose's own `expose:` key, container-network
    /// visibility, nothing to do with Traefik, so there's no router for
    /// it to need.
    #[test]
    fn a_port_with_no_router_needs_no_router() {
        let fields = ServiceFields {
            expose: Some(expose_port(8080)),
            ..Default::default()
        };
        let labels = compute("s", &fields, None, &bindings()).unwrap();
        assert!(labels.is_empty());
    }

    // --- per-router `middleware` (#221) ---

    /// #221's motivating case, byte for byte: a public router with no
    /// middleware beside an internal one carrying the IP allowlist. What
    /// used to force the whole label list into `raw`.
    #[test]
    fn two_routers_can_carry_different_middleware() {
        let public = router(Some("public"), Some("git.example.com"));
        let mut internal = router(Some("internal"), Some("git.internal.example.com"));
        internal.middleware = refs(&["local-ipwhitelist"]);
        let mut fields = with_routers(vec![public, internal]);
        fields.expose = Some(expose_port(3000));
        let labels = compute("gitea", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.gitea-public.rule=Host(`git.example.com`)",
                "traefik.http.routers.gitea-internal.rule=Host(`git.internal.example.com`)",
                "traefik.http.routers.gitea-internal.middlewares=local-ipwhitelist@file",
                "traefik.http.services.gitea.loadbalancer.server.port=3000",
            ]
        );
    }

    /// A router that names none emits no `middlewares=` label at all,
    /// exactly as an empty `entrypoint` emits no `entrypoints=` — and
    /// nothing on a *sibling* router leaks onto it.
    #[test]
    fn a_router_naming_no_middleware_emits_no_label() {
        let mut internal = router(Some("internal"), Some("a.example.local"));
        internal.middleware = refs(&["local-ipwhitelist"]);
        let mut fields = with_routers(vec![
            router(Some("public"), Some("a.example.com")),
            internal,
        ]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-public.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-internal.rule=Host(`a.example.local`)",
                "traefik.http.routers.app-internal.middlewares=local-ipwhitelist@file",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// Several entries on one router comma-join into the single
    /// `middlewares=` label, each with the file provider's `@file`
    /// suffix, in written order.
    #[test]
    fn router_middleware_entries_join_into_one_label_in_order() {
        let mut r = router(None, Some("a.example.com"));
        r.middleware = refs(&["local-ipwhitelist", "forwardAuth-authentik"]);
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels[1],
            "traefik.http.routers.app.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file"
        );
    }

    /// A comma inside one middleware name would splice an extra entry
    /// into the one comma-joined label, so it's refused — under
    /// `router.middleware`, the position it's actually written at.
    #[test]
    fn comma_in_a_router_middleware_is_rejected_under_its_own_field_name() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.middleware = refs(&["a,b"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.middleware",
                    character: ',',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A middleware needs a *host* as much as it needs a block: a
    /// `router` naming one but setting no host is the ordinary
    /// host-less-router error, reported before the middleware is read.
    /// This is what #80's router-less check turned into once the field
    /// moved inside the block (#221).
    #[test]
    fn a_hostless_router_with_middleware_is_still_a_hostless_router() {
        let mut r = router(Some("api"), None);
        r.middleware = refs(&["forwardAuth-authentik"]);
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterWithoutHost { ref router, .. }
                    if router.as_deref() == Some("api")
            ),
            "got {err:?}"
        );
    }
}
