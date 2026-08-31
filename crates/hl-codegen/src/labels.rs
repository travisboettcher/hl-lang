//! Computes one service's Traefik labels. These live on the service's
//! own `labels:` list, not a separate config file — Traefik's Docker
//! provider reads labels off container metadata directly, confirmed
//! against every real Docker-based service in the homelab this targets.

use std::collections::HashMap;

use hl_parser::{Ident, Literal, MatchExpr, Router, ServiceFields, Span, matchers};

use crate::{CodegenError, interp};

/// Characters rejected in every label value the user writes directly —
/// a router's `host` and each `entrypoints` entry. Motivated by `host`,
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
/// `entrypoints` shares this exact set, `,` included. It used to
/// need its own copy with `,` carved out, because a single scalar
/// `entrypoint "web,websecure"` was the only way to attach a router to
/// more than one entry point. Now that `entrypoints` is a list and
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
/// `traefik { disable }` since #221 folded `middleware` inside
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

/// Which of Traefik's two label namespaces a router lives in (#225).
///
/// Not a cosmetic prefix swap: the two namespaces have different rule
/// grammars, since a TCP router matches on the TLS handshake's server
/// name rather than on an HTTP request it never sees. [`Self::keyword`]
/// is what encodes that difference — everything else about a router's
/// labels is spelled identically either side, which is why one
/// `push_router_labels` serves both.
#[derive(Clone, Copy, PartialEq)]
enum Protocol {
    Http,
    Tcp,
}

impl Protocol {
    /// The label-namespace segment: `traefik.<segment>.routers.*` and
    /// `traefik.<segment>.services.*`.
    fn segment(self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Tcp => "tcp",
        }
    }

    /// The rule matcher a host goes inside. Traefik's TCP routers have
    /// no `Host()` — there is no Host header at that layer — and match
    /// `HostSNI()` against the TLS server name instead, for which `*`
    /// is the legal "any" wildcard that `Host()` has no equivalent of.
    fn host_matcher(self) -> &'static str {
        match self {
            Protocol::Http => "Host",
            Protocol::Tcp => "HostSNI",
        }
    }
}

/// Reads a router's `protocol` (#225), defaulting to HTTP.
///
/// Validated here rather than at parse time so a template can write
/// `protocol: $proto`: substitution runs after parsing, and an
/// unresolved `$proto` rejected as an unknown protocol would be a
/// diagnostic about the wrong thing. By the time codegen runs, every
/// `$param` is resolved (see [`hl_parser::Literal::Param`]), so what
/// reaches here is what the user actually meant.
fn router_protocol(service_name: &str, router: &Router) -> Result<Protocol, CodegenError> {
    let Some(lit) = router.protocol.as_ref() else {
        return Ok(Protocol::Http);
    };
    match lit.text() {
        "http" => Ok(Protocol::Http),
        "tcp" => Ok(Protocol::Tcp),
        other => Err(CodegenError::UnknownRouterProtocol {
            service: service_name.to_string(),
            protocol: other.to_string(),
            span: lit.span(),
        }),
    }
}

/// Lowers the `host`/`path_prefix` sugar into the [`MatchExpr`] a
/// written-out `rule` would have produced (#228).
///
/// Those two fields say one thing: a host match, with `path_prefix`'s
/// alternatives `&&`-ed on as one parenthesized `||` group when there
/// are any (#184). Building that as an expression rather than as a
/// string is what leaves [`render_rule`] the single rule-rendering path
/// — the sugar and the whole-rule spelling can't drift apart into two
/// slightly different ideas of how a rule is escaped, interpolated, or
/// parenthesized, because after this function there is only one of them.
///
/// The [`MatchExpr::Group`] around the prefixes is not optional and not
/// cosmetic: `&&` binds tighter than `||` in Traefik's rule grammar, so
/// ``Host(`h`) && PathPrefix(`a`) || PathPrefix(`b`)`` matches *any*
/// host under `/b`. It's emitted for a single prefix too, where it
/// changes nothing, because a rule whose shape depends on how many
/// prefixes it happens to have is a rule nobody can read off the source
/// — and keeping it a real node here is what preserves that, since
/// [`render_rule`] would otherwise drop a parenthesis precedence doesn't
/// require.
///
/// Every composite node here carries the span of the field it was built
/// from rather than a computed range over its children, because nothing
/// ever reads it: a diagnostic about a synthesized rule reports on the
/// matcher *argument's* span — the `host` or `path_prefix` entry the user
/// actually wrote — and [`CodegenError::RouterRuleAndHost`]'s own
/// `rule_span` only ever comes from a parsed `rule`, never from sugar.
/// Computing a range no diagnostic can quote would be unobservable work,
/// and unobservable work is untestable by definition.
fn sugar_expr(protocol: Protocol, host: &Literal, path_prefixes: &[Literal]) -> MatchExpr {
    let host_match = matcher_node(protocol.host_matcher(), host);
    let Some((first, rest)) = path_prefixes.split_first() else {
        return host_match;
    };
    let mut prefixes = matcher_node("PathPrefix", first);
    for prefix in rest {
        prefixes = MatchExpr::Or {
            lhs: Box::new(prefixes),
            rhs: Box::new(matcher_node("PathPrefix", prefix)),
            span: prefix.span(),
        };
    }
    let group = MatchExpr::Group {
        inner: Box::new(prefixes),
        span: first.span(),
    };
    MatchExpr::And {
        lhs: Box::new(host_match),
        rhs: Box::new(group),
        span: host.span(),
    }
}

/// One synthesized `Name(arg)` node for [`sugar_expr`], borrowing the
/// argument's own span so a diagnostic about it still points at the
/// `host`/`path_prefix` entry the user actually wrote.
fn matcher_node(name: &'static str, arg: &Literal) -> MatchExpr {
    MatchExpr::Matcher {
        name: Ident {
            name: name.to_string(),
            span: arg.span(),
        },
        args: vec![arg.clone()],
        span: arg.span(),
    }
}

/// How tightly a node binds, for [`render_rule`]'s parenthesization.
/// Traefik's own precedence: `!` tightest, then `&&`, then `||`.
fn precedence(expr: &MatchExpr) -> u8 {
    match expr {
        MatchExpr::Or { .. } => 0,
        MatchExpr::And { .. } => 1,
        MatchExpr::Not { .. } => 2,
        MatchExpr::Matcher { .. } | MatchExpr::Group { .. } => 3,
    }
}

/// Renders a rule expression into the text of a `.rule=` label (#228).
///
/// Parentheses are emitted in exactly two cases: where a
/// [`MatchExpr::Group`] says the source wrote them, and where precedence
/// would otherwise change what the rule means (an `||` under an `&&`, or
/// either under a `!`). Everything else is left bare, so an expression
/// written without parentheses renders without them and reparses in
/// Traefik as the same tree it parsed as here.
///
/// Each argument is interpolated and then checked, in that order and for
/// the reason every other label value is: `{{name}}` resolves to the text
/// that actually reaches the label, so the resolved text is what the
/// metacharacter guard has to inspect. That guard is the whole of what
/// stops rule injection here — Traefik delimits a matcher argument with a
/// backtick and offers no escape for one, so an argument holding a
/// backtick doesn't corrupt this matcher, it closes it and writes a
/// second (#65).
fn render_rule(
    expr: &MatchExpr,
    protocol: Protocol,
    service_name: &str,
    router: &Router,
    bindings: &HashMap<&str, &str>,
) -> Result<String, CodegenError> {
    match expr {
        MatchExpr::Matcher { name, args, .. } => {
            let allowed = match matchers::lookup(&name.name) {
                // A matcher the parser accepted is in the table by
                // construction; the sugar's own synthesized nodes are
                // too. `None` is therefore unreachable through any real
                // path, and is treated as "not legal here" rather than
                // panicking, on the same reasoning
                // `CodegenError::UnsubstitutedParameter` carries: a gap
                // in a compiler invariant should degrade to a located
                // message, never take the process down.
                Some(m) => match protocol {
                    Protocol::Http => m.http,
                    Protocol::Tcp => m.tcp,
                },
                None => false,
            };
            if !allowed {
                return Err(CodegenError::MatcherWrongProtocol {
                    service: service_name.to_string(),
                    router: router.key().map(str::to_string),
                    matcher: name.name.clone(),
                    protocol: protocol.segment(),
                    span: name.span,
                });
            }
            let mut rendered = Vec::with_capacity(args.len());
            for arg in args {
                let resolved = interp::resolve(arg.text(), bindings, arg.span())?;
                reject_metacharacters(&resolved, "router.rule", LABEL_METACHARACTERS, arg.span())?;
                rendered.push(format!("`{resolved}`"));
            }
            Ok(format!("{}({})", name.name, rendered.join(", ")))
        }
        MatchExpr::Group { inner, .. } => Ok(format!(
            "({})",
            render_rule(inner, protocol, service_name, router, bindings)?
        )),
        MatchExpr::Not { operand, .. } => {
            let inner = render_rule(operand, protocol, service_name, router, bindings)?;
            if precedence(operand) < precedence(expr) {
                Ok(format!("!({inner})"))
            } else {
                Ok(format!("!{inner}"))
            }
        }
        MatchExpr::And { lhs, rhs, .. } | MatchExpr::Or { lhs, rhs, .. } => {
            let op = if matches!(expr, MatchExpr::And { .. }) {
                "&&"
            } else {
                "||"
            };
            let mut parts = Vec::with_capacity(2);
            for side in [lhs, rhs] {
                let text = render_rule(side, protocol, service_name, router, bindings)?;
                if precedence(side) < precedence(expr) {
                    parts.push(format!("({text})"));
                } else {
                    parts.push(text);
                }
            }
            Ok(format!("{} {op} {}", parts[0], parts[1]))
        }
    }
}

/// The one `entrypoints=` label for `entrypoints`, or `None` when the
/// list is empty — Traefik's own way of saying "attach to every entry
/// point," rather than a homelab-specific default the compiler picks.
///
/// Interpolation runs per entry, before validation, for the reason it
/// always has: `{{name}}` is resolved here, so the resolved text is what
/// actually reaches the label and therefore what the metacharacter guard
/// has to inspect. An entry point spelled as a string (`entrypoints
/// "{{name}}-secure"`) is the case that makes this observable.
fn entrypoints_label(
    ns: &str,
    id: &str,
    field: &'static str,
    entrypoints: &[Literal],
    bindings: &HashMap<&str, &str>,
) -> Result<Option<String>, CodegenError> {
    if entrypoints.is_empty() {
        return Ok(None);
    }
    let mut eps = Vec::with_capacity(entrypoints.len());
    for r in entrypoints {
        let resolved = interp::resolve(r.text(), bindings, r.span())?;
        reject_metacharacters(&resolved, field, LABEL_METACHARACTERS, r.span())?;
        eps.push(resolved);
    }
    Ok(Some(format!(
        "traefik.{ns}.routers.{id}.entrypoints={}",
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
fn middlewares_label(
    ns: &str,
    id: &str,
    middleware: &[Literal],
) -> Result<Option<String>, CodegenError> {
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
        "traefik.{ns}.routers.{id}.middlewares={}",
        mws.join(",")
    )))
}

/// The one [`MatchExpr`] a router's rule comes from, whichever of the
/// two spellings wrote it (#228).
///
/// `rule` is used as written. `host`/`path_prefix` are lowered by
/// [`sugar_expr`] into the expression they have always meant. Writing
/// both is [`CodegenError::RouterRuleAndHost`]: two descriptions of one
/// rule, with nothing in the language to say which wins, and no reading
/// of "match this, and also match that instead" a user could have meant.
/// Writing neither is [`CodegenError::RouterWithoutHost`] — the block
/// that exists only to *be* a router saying nothing about which requests
/// reach it.
///
/// The sugar's own literals are resolved and checked here, under the
/// field names the user actually wrote (`router.host`,
/// `router.path_prefix`), rather than being left for [`render_rule`]'s
/// identical guard to catch under `router.rule`. A diagnostic about a
/// backtick in a `host` should say `host`. [`render_rule`] then re-runs
/// both on text that has already passed them, which is a no-op by
/// construction: interpolation can't reappear in resolved text (`{` is
/// itself a rejected metacharacter), and a value that cleared the guard
/// clears it again.
fn router_rule_expr(
    service_name: &str,
    router: &Router,
    protocol: Protocol,
    bindings: &HashMap<&str, &str>,
) -> Result<MatchExpr, CodegenError> {
    if let Some(rule) = &router.rule {
        // `host` first when a block wrote both, since it's the half a
        // reader is likelier to have meant to keep.
        let sugar = router
            .host
            .as_ref()
            .map(|h| ("host", h.span()))
            .or_else(|| {
                router
                    .path_prefix
                    .first()
                    .map(|p| ("path_prefix", p.span()))
            });
        if let Some((field, span)) = sugar {
            return Err(CodegenError::RouterRuleAndHost {
                service: service_name.to_string(),
                router: router.key().map(str::to_string),
                field,
                span,
                rule_span: rule.span(),
            });
        }
        return Ok(rule.clone());
    }

    let Some(host_lit) = &router.host else {
        return Err(CodegenError::RouterWithoutHost {
            service: service_name.to_string(),
            router: router.key().map(str::to_string),
            span: router.span,
        });
    };
    let host = interp::resolve(host_lit.text(), bindings, host_lit.span())?;
    reject_metacharacters(&host, "router.host", LABEL_METACHARACTERS, host_lit.span())?;

    // A TCP router has no paths to match on — there is no request URI at
    // that layer — so a `path_prefix` beside one is a contradiction
    // rather than something to silently drop (#225). The `rule` spelling
    // reaches the same conclusion through
    // [`CodegenError::MatcherWrongProtocol`], which can name the matcher;
    // here the field is the more useful thing to point at.
    if protocol == Protocol::Tcp
        && let Some(first) = router.path_prefix.first()
    {
        return Err(CodegenError::TcpRouterWithHttpOnlyField {
            service: service_name.to_string(),
            router: router.key().map(str::to_string),
            field: "path_prefix",
            span: first.span(),
        });
    }

    let mut prefixes = Vec::with_capacity(router.path_prefix.len());
    for prefix in &router.path_prefix {
        let resolved = interp::resolve(prefix.text(), bindings, prefix.span())?;
        reject_metacharacters(
            &resolved,
            "router.path_prefix",
            LABEL_METACHARACTERS,
            prefix.span(),
        )?;
        prefixes.push(Literal::Str(resolved, prefix.span()));
    }

    Ok(sugar_expr(
        protocol,
        &Literal::Str(host, host_lit.span()),
        &prefixes,
    ))
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
    let protocol = router_protocol(service_name, router)?;
    let ns = protocol.segment();

    let expr = router_rule_expr(service_name, router, protocol, bindings)?;
    labels.push(format!(
        "traefik.{ns}.routers.{id}.rule={}",
        render_rule(&expr, protocol, service_name, router, bindings)?
    ));
    if let Some(label) =
        entrypoints_label(ns, &id, "router.entrypoints", &router.entrypoints, bindings)?
    {
        labels.push(label);
    }
    if let Some(label) = middlewares_label(ns, &id, &router.middleware)? {
        labels.push(label);
    }
    if let Some(priority) = &router.priority {
        labels.push(format!(
            "traefik.{ns}.routers.{id}.priority={}",
            priority.text()
        ));
    }

    // A router naming its own port gets a Traefik *service* of its own,
    // keyed by the router's id, and says so with a `.service=` label
    // (#225). Without one it falls through to the single service-wide
    // target `compute` emits from `expose.port`, which is what every
    // router shared before #225 and what a file written against that
    // model still gets.
    match &router.port {
        Some(port) => {
            labels.push(format!("traefik.{ns}.routers.{id}.service={id}"));
            labels.push(format!(
                "traefik.{ns}.services.{id}.loadbalancer.server.port={}",
                port.text()
            ));
        }
        None if protocol == Protocol::Tcp => {
            // The fallback target is an *HTTP* service, so there is
            // nothing for a TCP router to fall back to.
            return Err(CodegenError::TcpRouterWithoutPort {
                service: service_name.to_string(),
                router: router.key().map(str::to_string),
                span: router.span,
            });
        }
        None => {}
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
/// `.entrypoints=` (if the block's `entrypoints` list is non-empty — one
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
/// A service that sets `traefik { disable }` (#159) short-circuits all
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
        && let Some(disabled_span) = traefik.disable
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

    // The one service-wide load-balancer target, from `expose.port` —
    // emitted only for the routers that actually fall back to it.
    //
    // Through #224 that was every router: a container was assumed to
    // listen on one port however many routers pointed at it, so the
    // label went out whenever the service had any router at all. #225
    // let a router name its own port, which splits the question in two.
    // A service whose routers all name their own needs no shared target
    // and no `expose` either, since there is no longer one port that is
    // "the" port. A service with even one router still falling back
    // needs both, and a missing `expose` there is still
    // `RouterWithoutPort` — the live defect #198 closed, where a routed
    // service silently emitted no `loadbalancer.server.port` at all.
    // Every router still lacking a port at this point is an HTTP one:
    // the loop above has already refused a portless TCP router, which
    // has nothing to fall back to (`TcpRouterWithoutPort`). So the scan
    // asks about the port alone rather than re-deriving the protocol.
    let fallback = fields.routers.iter().find(|r| r.port.is_none());
    if let Some(router) = fallback {
        match fields.expose.as_ref().and_then(|e| e.port.as_ref()) {
            Some(port) => labels.push(format!(
                "traefik.http.services.{service_name}.loadbalancer.server.port={}",
                port.text()
            )),
            None => {
                return Err(CodegenError::RouterWithoutPort {
                    service: service_name.to_string(),
                    span: router.span,
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

    /// A numeric literal, for the router sub-fields #225 gave numbers
    /// (`priority`, `port`) — the shape `expose.port` already had.
    fn num(value: u64) -> Literal {
        Literal::Number {
            text: value.to_string(),
            value,
            span: span(),
        }
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
            entrypoints: Vec::new(),
            path_prefix: Vec::new(),
            middleware: Vec::new(),
            priority: None,
            port: None,
            protocol: None,
            rule: None,
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
    fn newline_in_an_entrypoints_entry_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoints = refs(&["web\nsecure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoints",
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
    fn backtick_in_an_entrypoints_entry_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoints = refs(&["web`-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoints",
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
    fn comma_in_a_single_entrypoints_entry_is_rejected() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoints = refs(&["web,web-secure"]);
        let err = compute("s", &fields, None, &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoints",
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
        fields.routers[0].entrypoints = refs(&["web", "web-secure"]);
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
        fields.routers[0].entrypoints = refs(&["web", "web-secure", "metrics"]);
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
    fn entrypoints_are_checked_after_interpolation() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoints = refs(&["{{name}}-secure"]);
        let bindings = HashMap::from([("name", "web`")]);
        let err = compute("s", &fields, None, &bindings).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsafeLabelValue {
                field: "router.entrypoints",
                character: '`',
                ..
            }
        ));
    }

    /// And the resolved text — not the raw `{{name}}` source — is what
    /// lands in the label.
    #[test]
    fn entrypoints_are_interpolated_into_the_label() {
        let mut fields = router_with_host("ok.example.com");
        fields.routers[0].entrypoints = refs(&["{{name}}-secure"]);
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
        r.entrypoints = refs(&["web-secure"]);
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

    // --- traefik { disable } (#159) ---

    fn disabled_traefik() -> Traefik {
        Traefik {
            disable: Some(span()),
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
    /// contradict `disable`.
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
    /// when `fields.traefik.disable` is actually `Some`.
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
        api.entrypoints = refs(&["web-secure"]);
        api.path_prefix = prefixes(&["/api/v1", "/dav/", "/.well-known/"]);
        let mut api_local = router(Some("api-local"), Some("vikunja.techdebtor.local"));
        api_local.entrypoints = refs(&["local"]);
        api_local.path_prefix = prefixes(&["/api/v1", "/dav/", "/.well-known/"]);
        let mut frontend = router(Some("frontend"), Some("vikunja.techdebtor.io"));
        frontend.entrypoints = refs(&["web-secure"]);
        let mut frontend_local = router(Some("frontend-local"), Some("vikunja.techdebtor.local"));
        frontend_local.entrypoints = refs(&["local"]);

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
        first.entrypoints = refs(&["web-secure"]);
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
    fn router_entrypoints_without_a_host_is_rejected() {
        let mut r = router(Some("api"), None);
        r.entrypoints = refs(&["web-secure"]);
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
    fn comma_in_a_router_entrypoints_entry_is_rejected() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.entrypoints = refs(&["web,web-secure"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.entrypoints",
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

    /// `traefik { disable }` covers `router` blocks too: a whole router
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
        r.entrypoints = refs(&["web-secure"]);
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
    /// exactly as an empty `entrypoints` emits no `entrypoints=` — and
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

    // --- `priority`, per-router `port`, and TCP routers (#225) ---

    /// #225's motivating label set, byte for byte: two HTTP routers
    /// sharing a host and separated only by `priority`, each pointing at
    /// its own port through its own Traefik service, beside a TCP router
    /// in the other namespace entirely. No `expose` anywhere — with
    /// every router naming its own port there is no "the" port for one
    /// to name.
    #[test]
    fn sftpgo_emits_two_http_services_and_one_tcp_router() {
        let mut web = router(Some("web"), Some("sftp.internal.techdebtor.io"));
        web.priority = Some(num(100));
        web.port = Some(num(2222));
        let mut webdav = router(Some("webdav"), Some("sftp.internal.techdebtor.io"));
        webdav.priority = Some(num(90));
        webdav.port = Some(num(4444));
        let mut sftp = router(Some("sftp"), Some("*"));
        sftp.protocol = Some(lit("tcp"));
        sftp.port = Some(num(1111));

        let fields = with_routers(vec![web, webdav, sftp]);
        let labels = compute("sftpgo", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.sftpgo-web.rule=Host(`sftp.internal.techdebtor.io`)",
                "traefik.http.routers.sftpgo-web.priority=100",
                "traefik.http.routers.sftpgo-web.service=sftpgo-web",
                "traefik.http.services.sftpgo-web.loadbalancer.server.port=2222",
                "traefik.http.routers.sftpgo-webdav.rule=Host(`sftp.internal.techdebtor.io`)",
                "traefik.http.routers.sftpgo-webdav.priority=90",
                "traefik.http.routers.sftpgo-webdav.service=sftpgo-webdav",
                "traefik.http.services.sftpgo-webdav.loadbalancer.server.port=4444",
                "traefik.tcp.routers.sftpgo-sftp.rule=HostSNI(`*`)",
                "traefik.tcp.routers.sftpgo-sftp.service=sftpgo-sftp",
                "traefik.tcp.services.sftpgo-sftp.loadbalancer.server.port=1111",
            ]
        );
    }

    /// A router naming no port still falls back to the one service-wide
    /// target, so a file written before #225 emits exactly what it did.
    #[test]
    fn a_router_without_its_own_port_still_uses_the_shared_target() {
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

    /// One router naming its own port leaves a sibling that doesn't on
    /// the shared target — the two coexist rather than one mode winning.
    #[test]
    fn own_port_and_shared_target_coexist() {
        let mut own = router(Some("api"), Some("a.example.com"));
        own.port = Some(num(9000));
        let mut fields = with_routers(vec![router(Some("web"), Some("b.example.com")), own]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-web.rule=Host(`b.example.com`)",
                "traefik.http.routers.app-api.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.service=app-api",
                "traefik.http.services.app-api.loadbalancer.server.port=9000",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }

    /// With every router naming its own port there is nothing to fall
    /// back to, so `expose` stops being required — the state #225
    /// describes, where no one port is "the" port.
    #[test]
    fn every_router_naming_a_port_needs_no_expose() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.port = Some(num(9000));
        let fields = with_routers(vec![r]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.service=app-api",
                "traefik.http.services.app-api.loadbalancer.server.port=9000",
            ]
        );
    }

    /// ...but one router still falling back keeps `expose` required,
    /// and the diagnostic points at *that* router rather than the first.
    #[test]
    fn a_single_falling_back_router_still_requires_expose() {
        let mut own = router(Some("api"), Some("a.example.com"));
        own.port = Some(num(9000));
        let fields = with_routers(vec![own, router(Some("web"), Some("b.example.com"))]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterWithoutPort { .. }),
            "got {err:?}"
        );
    }

    /// A TCP router's whole label group lives in the other namespace —
    /// `entrypoints` and `middlewares` included, which Traefik spells
    /// the same way under `traefik.tcp.*`.
    #[test]
    fn a_tcp_router_puts_every_label_in_the_tcp_namespace() {
        let mut r = router(Some("sftp"), Some("*"));
        r.protocol = Some(lit("tcp"));
        r.port = Some(num(1111));
        r.entrypoints = refs(&["sftp"]);
        r.middleware = refs(&["inflightconn"]);
        let fields = with_routers(vec![r]);
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.tcp.routers.app-sftp.rule=HostSNI(`*`)",
                "traefik.tcp.routers.app-sftp.entrypoints=sftp",
                "traefik.tcp.routers.app-sftp.middlewares=inflightconn@file",
                "traefik.tcp.routers.app-sftp.service=app-sftp",
                "traefik.tcp.services.app-sftp.loadbalancer.server.port=1111",
            ]
        );
    }

    /// An explicit `protocol: http` is the default said out loud, and
    /// changes nothing about the emitted labels.
    #[test]
    fn an_explicit_http_protocol_matches_the_default() {
        let mut explicit = router(None, Some("a.example.com"));
        explicit.protocol = Some(lit("http"));
        let mut with_explicit = with_routers(vec![explicit]);
        with_explicit.expose = Some(expose_port(80));
        let mut default = with_routers(vec![router(None, Some("a.example.com"))]);
        default.expose = Some(expose_port(80));
        assert_eq!(
            compute("app", &with_explicit, None, &bindings()).unwrap(),
            compute("app", &default, None, &bindings()).unwrap()
        );
    }

    /// A TCP router has no request path to match, so a `path_prefix`
    /// beside one is refused rather than silently dropped.
    #[test]
    fn a_tcp_router_rejects_path_prefix() {
        let mut r = router(Some("sftp"), Some("*"));
        r.protocol = Some(lit("tcp"));
        r.port = Some(num(1111));
        r.path_prefix = prefixes(&["/api"]);
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::TcpRouterWithHttpOnlyField {
                    field: "path_prefix",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// The shared fallback target is an HTTP service, so a TCP router
    /// can't reach it — it must name its own port even when the service
    /// has an `expose`.
    #[test]
    fn a_tcp_router_without_a_port_is_rejected_even_with_expose() {
        let mut r = router(Some("sftp"), Some("*"));
        r.protocol = Some(lit("tcp"));
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::TcpRouterWithoutPort { .. }),
            "got {err:?}"
        );
    }

    /// Anything but `http`/`tcp` is refused, naming both.
    #[test]
    fn an_unknown_protocol_is_rejected() {
        let mut r = router(Some("x"), Some("a.example.com"));
        r.protocol = Some(lit("udp"));
        r.port = Some(num(1));
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnknownRouterProtocol { ref protocol, .. } if protocol == "udp"
            ),
            "got {err:?}"
        );
    }

    /// `priority` rides between the middlewares and the service target,
    /// and is emitted verbatim — it's a number, checked as one during
    /// composition rather than reformatted here.
    #[test]
    fn priority_is_emitted_between_middlewares_and_the_service_target() {
        let mut r = router(Some("api"), Some("a.example.com"));
        r.middleware = refs(&["auth"]);
        r.priority = Some(num(100));
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        assert_eq!(
            labels,
            vec![
                "traefik.http.routers.app-api.rule=Host(`a.example.com`)",
                "traefik.http.routers.app-api.middlewares=auth@file",
                "traefik.http.routers.app-api.priority=100",
                "traefik.http.services.app.loadbalancer.server.port=80",
            ]
        );
    }
    // --- `router { rule: ... }` (#228) ---

    /// A matcher node, for building a rule by hand the way a `.hll`
    /// file's own `rule` would parse into one.
    fn matcher(name: &str, args: &[&str]) -> MatchExpr {
        MatchExpr::Matcher {
            name: Ident {
                name: name.to_string(),
                span: span(),
            },
            args: args.iter().map(|a| lit(a)).collect(),
            span: span(),
        }
    }

    fn and(lhs: MatchExpr, rhs: MatchExpr) -> MatchExpr {
        MatchExpr::And {
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span: span(),
        }
    }

    fn or(lhs: MatchExpr, rhs: MatchExpr) -> MatchExpr {
        MatchExpr::Or {
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span: span(),
        }
    }

    fn not(inner: MatchExpr) -> MatchExpr {
        MatchExpr::Not {
            operand: Box::new(inner),
            span: span(),
        }
    }

    fn group(inner: MatchExpr) -> MatchExpr {
        MatchExpr::Group {
            inner: Box::new(inner),
            span: span(),
        }
    }

    /// A router whose rule is written out rather than sugared.
    fn ruled(name: Option<&str>, rule: MatchExpr) -> Router {
        let mut r = router(name, None);
        r.rule = Some(rule);
        r
    }

    fn rule_of(r: Router) -> String {
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let labels = compute("app", &fields, None, &bindings()).unwrap();
        labels[0]
            .split_once(".rule=")
            .expect("the first label is the rule")
            .1
            .to_string()
    }

    /// #228's own case: the negation `path_prefix` could never express.
    #[test]
    fn a_negated_prefix_group_renders_as_the_issue_quotes_it() {
        let rule = and(
            matcher("Host", &["travel.internal.techdebtor.io"]),
            not(group(or(
                or(
                    or(
                        matcher("PathPrefix", &["/media"]),
                        matcher("PathPrefix", &["/admin"]),
                    ),
                    matcher("PathPrefix", &["/static"]),
                ),
                matcher("PathPrefix", &["/accounts"]),
            ))),
        );
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`travel.internal.techdebtor.io`) && !(PathPrefix(`/media`) || \
             PathPrefix(`/admin`) || PathPrefix(`/static`) || PathPrefix(`/accounts`))"
        );
    }

    /// Parentheses the source wrote survive; parentheses it didn't are
    /// not invented, since `&&` already binds tighter than `||`.
    #[test]
    fn precedence_alone_needs_no_parentheses() {
        let rule = or(
            matcher("Host", &["a.example.com"]),
            and(
                matcher("Host", &["b.example.com"]),
                matcher("Method", &["GET"]),
            ),
        );
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`a.example.com`) || Host(`b.example.com`) && Method(`GET`)"
        );
    }

    /// The other direction: an `||` under an `&&` *does* need them, or
    /// the rule would match every host under the last alternative — the
    /// exact bug `path_prefix`'s own group exists to prevent. The parser
    /// can't build this tree without a `Group` node, so this is the
    /// renderer being correct for an AST built any other way, the same
    /// belt-and-braces `reject_unsafe_router_name` applies to a router
    /// name the grammar already constrains.
    #[test]
    fn a_lower_precedence_child_is_parenthesized_even_without_a_group() {
        let rule = and(
            matcher("Host", &["a.example.com"]),
            or(
                matcher("PathPrefix", &["/api"]),
                matcher("PathPrefix", &["/dav"]),
            ),
        );
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`a.example.com`) && (PathPrefix(`/api`) || PathPrefix(`/dav`))"
        );
    }

    /// `!` binds tighter than both, so negating a binary node needs
    /// parentheses whether or not one was written.
    #[test]
    fn negating_a_binary_node_parenthesizes_it() {
        let rule = not(or(
            matcher("PathPrefix", &["/api"]),
            matcher("PathPrefix", &["/dav"]),
        ));
        assert_eq!(
            rule_of(ruled(None, rule)),
            "!(PathPrefix(`/api`) || PathPrefix(`/dav`))"
        );
    }

    /// Negating a single matcher needs none.
    #[test]
    fn negating_a_matcher_needs_no_parentheses() {
        let rule = not(matcher("PathPrefix", &["/admin"]));
        assert_eq!(rule_of(ruled(None, rule)), "!PathPrefix(`/admin`)");
    }

    /// Nor does negating another negation. `!` binds tighter than
    /// itself, so `!!x` is the one shape that tells "parenthesize a
    /// *lower*-precedence child" apart from "parenthesize an
    /// equal-or-lower one" — every other operand of a `!` is a matcher
    /// or a group, which binds tighter either way.
    #[test]
    fn negating_a_negation_needs_no_parentheses() {
        let rule = not(not(matcher("PathPrefix", &["/admin"])));
        assert_eq!(rule_of(ruled(None, rule)), "!!PathPrefix(`/admin`)");
    }

    /// The same distinction one level over, for the binary operators: a
    /// chain of one operator nests same-precedence nodes, and those need
    /// no parentheses either — only a genuinely lower-precedence child
    /// does.
    #[test]
    fn a_chain_of_one_operator_needs_no_parentheses() {
        let rule = and(
            and(matcher("Host", &["a"]), matcher("Method", &["GET"])),
            matcher("PathPrefix", &["/x"]),
        );
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`a`) && Method(`GET`) && PathPrefix(`/x`)"
        );

        let rule = or(
            or(matcher("Host", &["a"]), matcher("Host", &["b"])),
            matcher("Host", &["c"]),
        );
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`a`) || Host(`b`) || Host(`c`)"
        );
    }

    /// A two-argument matcher joins its arguments the way Traefik writes
    /// them, each in its own backticks.
    #[test]
    fn a_two_argument_matcher_backticks_each_argument() {
        let rule = matcher("Header", &["X-Env", "prod"]);
        assert_eq!(rule_of(ruled(None, rule)), "Header(`X-Env`, `prod`)");
    }

    /// `{{name}}` resolves inside a matcher argument, exactly as it does
    /// in a `host` — the resolved text is what reaches the label.
    #[test]
    fn a_matcher_argument_resolves_interpolation() {
        let rule = matcher("Host", &["{{name}}.internal.example.com"]);
        assert_eq!(
            rule_of(ruled(None, rule)),
            "Host(`syncthing.internal.example.com`)"
        );
    }

    /// And the metacharacter guard runs on that resolved text, since a
    /// backtick would close the matcher and write a second one (#65).
    #[test]
    fn a_backtick_in_a_matcher_argument_is_rejected() {
        let rule = matcher("Host", &["ok.example.com`) || HostRegexp(`{any:.+}"]);
        let mut fields = with_routers(vec![ruled(None, rule)]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::UnsafeLabelValue {
                    field: "router.rule",
                    character: '`',
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A TCP router's rule may not use an HTTP-only matcher — there is
    /// no request URI at that layer to match a path against.
    #[test]
    fn a_tcp_rule_rejects_an_http_only_matcher() {
        let mut r = ruled(Some("sftp"), matcher("PathPrefix", &["/files"]));
        r.protocol = Some(lit("tcp"));
        r.port = Some(num(1111));
        let fields = with_routers(vec![r]);
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::MatcherWrongProtocol {
                    protocol: "tcp",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// And the mirror image: `HostSNI` reads a TLS server name, which an
    /// HTTP router never sees.
    #[test]
    fn an_http_rule_rejects_a_tcp_only_matcher() {
        let mut fields = with_routers(vec![ruled(None, matcher("HostSNI", &["*"]))]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::MatcherWrongProtocol {
                    protocol: "http",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// `ClientIP` is the one matcher both namespaces have, so it works
    /// either side rather than being arbitrarily assigned to one.
    #[test]
    fn client_ip_is_legal_in_both_namespaces() {
        let mut fields = with_routers(vec![ruled(None, matcher("ClientIP", &["10.0.0.0/8"]))]);
        fields.expose = Some(expose_port(80));
        assert!(compute("app", &fields, None, &bindings()).is_ok());

        let mut r = ruled(Some("raw"), matcher("ClientIP", &["10.0.0.0/8"]));
        r.protocol = Some(lit("tcp"));
        r.port = Some(num(1111));
        let fields = with_routers(vec![r]);
        assert!(compute("app", &fields, None, &bindings()).is_ok());
    }

    /// A `rule` beside the sugar it replaces describes one rule twice,
    /// with nothing to say which wins.
    #[test]
    fn a_rule_beside_a_host_is_rejected() {
        let mut r = ruled(None, matcher("Host", &["a.example.com"]));
        r.host = Some(lit("b.example.com"));
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterRuleAndHost { field: "host", .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_rule_beside_a_path_prefix_is_rejected() {
        let mut r = ruled(None, matcher("Host", &["a.example.com"]));
        r.path_prefix = prefixes(&["/api"]);
        let mut fields = with_routers(vec![r]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(
                err,
                CodegenError::RouterRuleAndHost {
                    field: "path_prefix",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A router with neither spelling still has no rule to emit, and the
    /// message now names both ways to give it one.
    #[test]
    fn a_router_with_neither_host_nor_rule_is_rejected() {
        let mut fields = with_routers(vec![router(Some("api"), None)]);
        fields.expose = Some(expose_port(80));
        let err = compute("app", &fields, None, &bindings()).unwrap_err();
        assert!(
            matches!(err, CodegenError::RouterWithoutHost { .. }),
            "got {err:?}"
        );
    }
}
