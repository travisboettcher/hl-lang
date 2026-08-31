use hl_lexer::Span;

/// A parsed hl-lang source file: a sequence of top-level declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub decls: Vec<TopDecl>,
}

/// One top-level declaration. `Service`/`TemplateDecl` are boxed since
/// they're much larger than `Network`/`Volume` (many optional/list
/// fields) — keeps `TopDecl` itself small to pass around.
#[derive(Debug, Clone, PartialEq)]
pub enum TopDecl {
    Network(Network),
    Volume(Volume),
    Service(Box<Service>),
    Template(Box<TemplateDecl>),
    Use(UseDecl),
}

/// `use "path/to/file.hll" as alias` — imports another file's top-level
/// `network`/`template` declarations under a local alias, referenced
/// elsewhere as `alias.name` (see [`Literal::qualifier`] and
/// [`TemplateInvocation::qualifier`]). Purely syntactic: `parse()` never
/// touches the filesystem or validates that `path` resolves to anything
/// real — resolving the path and loading the target file is a later
/// stage's job.
#[derive(Debug, Clone, PartialEq)]
pub struct UseDecl {
    /// Always `Literal::Str` once parsed — `use`'s path must be a quoted
    /// string, since `IDENT`'s grammar can't represent `.`/`/` at all.
    pub path: Literal,
    pub alias: Ident,
    pub span: Span,
}

/// The one value grammar the whole language shares (#196): `value ::=
/// STRING | NUMBER | IDENT | IDENT "." IDENT | "$" IDENT`. Before #196
/// this was two separate types — `Literal` (`STRING | NUMBER | IDENT |
/// "$" IDENT`, no qualifier) and a `Reference` struct (`IDENT ( "."
/// IDENT )?`, no `$param`) — purely because neither could represent the
/// other's extra bit. That split meant a `Reference`-typed position
/// (`networks`, `dns`, `env_file`, `expose.entrypoint`,
/// `router.entrypoints`, `router.middleware`, a `depends_on` entry, a
/// named-volume mount's
/// host side) could never accept a `$param`: `template web(net)
/// { networks [$net] }` was a parse error with no way to fix it short of
/// giving `networks` a second grammar. Folding the qualifier into this
/// type is what closes that gap — a `Reference` is now just this type's
/// [`Self::Qualified`] variant, so every position that used to parse one
/// gets `$param` for free.
///
/// Whether the *qualified* form is legal at a given position is a
/// semantic question, not a syntactic one any more: the parser always
/// attempts it wherever it attempts a reference at all, and
/// `compose::reject_qualified` (driven by
/// [`crate::schema::allows_qualified_reference`]) rejects it
/// post-parse, with [`crate::compose::ComposeError::UnsupportedQualifiedReference`],
/// everywhere but `networks` and a named-volume mount's host side — the
/// two positions with a real cross-file declaration to resolve one
/// against. Whether `$param` is legal is likewise not this type's
/// concern: that's [`crate::ParseError::ParamReferenceOutsideTemplate`],
/// checked the moment the `$` is seen, uniformly across every position
/// this type appears in.
///
/// The kind (string/number/bare identifier) is preserved rather than
/// normalized to a plain string, since the grammar allows any of the
/// three wherever a value is expected — e.g. `restart unless-stopped`
/// (bare `Ident`) and `restart "unless-stopped"` (`Str`) are both legal
/// and mean the same thing downstream, but keeping the distinction costs
/// nothing and supports future provenance/pretty-printing needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String, Span),
    /// `text` is the exact source digits (preserved for provenance, e.g.
    /// leading zeros); `value` is `text` parsed as `u64` — the parser
    /// rejects anything that doesn't fit via
    /// [`crate::ParseError::NumberOutOfRange`], so `value` is always
    /// valid once a `Literal::Number` exists.
    Number {
        text: String,
        value: u64,
        span: Span,
    },
    Ident(String, Span),
    /// `alias.name` — a reference qualified by a `use`-imported file's
    /// local alias (see [`UseDecl`]). Only ever produced from a bare
    /// `IDENT` token followed by `.` `IDENT`; a `STRING` key's content is
    /// just string content, never followed by a structural `.`, so this
    /// variant's `qualifier`/`name` are always themselves plain
    /// identifiers, never string-derived.
    ///
    /// Boxed for the same reason [`TopDecl::Service`]/[`TopDecl::Template`]
    /// are: an `Ident` plus a second `String` plus two `Span`s makes this
    /// the largest payload any `Literal` variant carries by a wide margin
    /// (more than double [`Self::Number`]'s, the next largest), and since
    /// an enum's size is its largest variant's, an unboxed `Qualified`
    /// would inflate every `Literal`/`Option<Literal>` slot in the AST —
    /// which is most of it — to pay for a variant the overwhelming
    /// majority of literals never use. That inflation is exactly what
    /// reproduced #72's stack-overflow bug class in a new place: template
    /// composition is mutually recursive over `ServiceFields`-shaped
    /// values (see `compose::MAX_TEMPLATE_DEPTH`'s own doc), so a bigger
    /// `Literal` means a bigger stack frame at *every* recursion level,
    /// not a one-time cost — enough, unboxed, to overflow a debug build's
    /// default thread stack barely a third of the way to
    /// `MAX_TEMPLATE_DEPTH`, well short of the margin that constant's own
    /// doc promises.
    Qualified(Box<QualifiedRef>),
    /// A `$name` parameter reference inside a `template`'s own body,
    /// naming one of that *same* template's own declared parameters,
    /// e.g. `$puid` in `template linuxserver_app(puid, pgid) { env PUID =
    /// $puid }`. Produced directly by the parser
    /// when it sees the `$` sigil — never by ordinary literal parsing,
    /// and never legal (a parse error) outside a template body, since a
    /// plain `service` isn't parameterized. Composition
    /// ([`fn@crate::compose`]) substitutes every `Param` with the
    /// invocation's bound argument value; a `Param` surviving composition
    /// would be a bug.
    Param(String, Span),
}

impl Literal {
    /// The literal's text content, regardless of which kind it is. For
    /// [`Self::Qualified`], this is the unqualified `name` half alone —
    /// the qualifier names the file the declaration lives in, not the
    /// declaration itself, exactly as [`ArrowMapHost::text`] already
    /// documented before the two types merged.
    pub fn text(&self) -> &str {
        match self {
            Literal::Str(s, _) | Literal::Ident(s, _) | Literal::Param(s, _) => s,
            Literal::Number { text, .. } => text,
            Literal::Qualified(q) => &q.name,
        }
    }

    /// The literal's location in source. For [`Self::Qualified`], this
    /// covers the whole `alias.name`, not just the trailing name — see
    /// [`QualifiedRef::name_span`] for the narrower span.
    pub fn span(&self) -> Span {
        match self {
            Literal::Str(_, span) | Literal::Ident(_, span) | Literal::Param(_, span) => *span,
            Literal::Number { span, .. } => *span,
            Literal::Qualified(q) => q.span,
        }
    }

    /// The `alias` in an `alias.name` reference, or `None` for every
    /// other kind. The single question
    /// `compose::reject_qualified`/`resolve_qualified_references`
    /// ask of every reference-shaped position — see [`Self`]'s own doc.
    pub fn qualifier(&self) -> Option<&Ident> {
        match self {
            Literal::Qualified(q) => Some(&q.qualifier),
            _ => None,
        }
    }
}

/// [`Literal::Qualified`]'s boxed-out payload — see that variant's own
/// doc for why it's boxed rather than inline.
#[derive(Debug, Clone, PartialEq)]
pub struct QualifiedRef {
    pub qualifier: Ident,
    pub name: String,
    /// The span of just the trailing `name` segment, after the `.`.
    /// [`Literal::span`] covers the whole reference including the
    /// qualifier.
    pub name_span: Span,
    pub span: Span,
}

/// One declared template parameter: just its name. Parameters carried an
/// optional `: Number|String` type annotation before #201, checked
/// strictly against the argument's own literal kind at the call site —
/// dropped once #196 let `$param` reach reference and list positions the
/// two-type vocabulary had no way to describe (see docs/DESIGN.md's
/// Syntactic grammar section for why growing the annotation vocabulary
/// to match lost out to checking the argument against the field it
/// substitutes into instead). A parameter now accepts any literal kind
/// at the call site; [`crate::compose::ComposeError::ArgumentNotReferenceShaped`]
/// is what a reference-shaped field enforces once the argument lands.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Ident,
}

/// One of Compose's own three `depends_on` readiness conditions (#155):
/// wait for the target container to merely start (`ServiceStarted` —
/// what a plain `depends_on [db]` has always meant, and still means
/// when no entry names a condition at all), wait for its `healthcheck`
/// to report healthy (`ServiceHealthy` — only meaningful when the
/// target actually declares one, whether via `hll`'s own `healthcheck`
/// field or a `HEALTHCHECK` baked into its image, which `hll` has no
/// visibility into either way — see [`ServiceFields::depends_on`]'s doc
/// for why that's deliberately not checked here), or wait for it to
/// exit zero (`ServiceCompletedSuccessfully` — a one-shot init/migration
/// container). Exactly Compose's own three; nothing else is legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependsOnCondition {
    ServiceStarted,
    ServiceHealthy,
    ServiceCompletedSuccessfully,
}

impl DependsOnCondition {
    /// Parses one of Compose's three condition spellings, exactly as
    /// they appear in `.hll` source and in the generated YAML alike —
    /// `hll` doesn't rename or abbreviate any of them. `None` for
    /// anything else, which the caller turns into
    /// [`crate::ParseError::InvalidDependsOnCondition`].
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "service_started" => Some(Self::ServiceStarted),
            "service_healthy" => Some(Self::ServiceHealthy),
            "service_completed_successfully" => Some(Self::ServiceCompletedSuccessfully),
            _ => None,
        }
    }

    /// The exact string Compose's own `condition:` key expects, and the
    /// same string [`Self::parse`] accepts back — `hll` never renames a
    /// Compose keyword on its way through.
    pub fn compose_value(self) -> &'static str {
        match self {
            Self::ServiceStarted => "service_started",
            Self::ServiceHealthy => "service_healthy",
            Self::ServiceCompletedSuccessfully => "service_completed_successfully",
        }
    }
}

/// One `depends_on`-list entry (#155): a plain same-file service
/// reference (`db`), or a reference plus an explicit readiness
/// condition (`db { condition: service_healthy }`). `condition` also
/// carries the span of just the condition value, separate from the
/// entry's own `span`, so a future diagnostic can point at exactly the
/// keyword rather than the whole entry.
///
/// Not a [`TemplateInvocation`] with a schema-free [`RawMap`] argument
/// bag, even though the two share the same "`IDENT` optionally followed
/// by a `{ }` body" shape: `condition` is this body's one and only
/// legal key, and its value is one of exactly three fixed Compose
/// keywords rather than an arbitrary one, checked immediately at parse
/// time (see [`crate::ParseError::InvalidDependsOnCondition`]) rather
/// than deferred to composition the way a template argument's binding
/// is.
#[derive(Debug, Clone, PartialEq)]
pub struct DependsOnEntry {
    pub reference: Literal,
    pub condition: Option<(DependsOnCondition, Span)>,
    pub span: Span,
}

impl DependsOnEntry {
    /// This entry's condition as Compose itself would read it: a bare
    /// entry (`condition: None`) means exactly what an explicit
    /// `condition: service_started` means, since that's Compose's own
    /// implicit default for `depends_on`. Used both by composition's
    /// merge (to tell "two entries that agree" apart from "two entries
    /// that genuinely conflict," regardless of which one spelled the
    /// condition out — see `compose.rs`'s `merge_depends_on`) and by
    /// codegen (to fill in the long map form's value for an entry that
    /// left `condition` unset).
    pub fn effective_condition(&self) -> DependsOnCondition {
        self.condition
            .map_or(DependsOnCondition::ServiceStarted, |(c, _)| c)
    }
}

/// The name a `network` or `service` declaration is given.
#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// A parsed `network` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub name: Ident,
    /// `Some(span)` of the bare `external` flag if it was set; `None`
    /// otherwise (there is no way to write `external: false` this
    /// milestone — the field is bare-presence only).
    pub external: Option<Span>,
    /// The real underlying Docker network name, set via the `name`
    /// field (`network traefik-net { name: "docker_default" }`), when it
    /// differs from `name.name` above. `None` means "use `name.name`
    /// verbatim" — codegen, not the parser, applies that default,
    /// mirroring how [`Image::reference`] staying `Option` defers
    /// "required" enforcement to a later stage.
    pub real_name: Option<Literal>,
    pub span: Span,
}

/// A parsed top-level `volume` declaration — the named Docker volume a
/// service's `volume name -> "/path"` entry refers to. Deliberately the
/// same shape as [`Network`] (`external` flag plus an optional
/// `real_name` override), since the two answer the same question about
/// two different Compose top-level sections; `driver`/`driver_opts` are
/// the extra Compose knobs that only exist on the volume side.
///
/// Note the *field* named `volume` inside a `service`/`template` body is
/// a different, map-kind schema ([`ArrowMap`]) that happens to share
/// the identifier — a top-level `volume x { ... }` declares the volume,
/// a service-level `volume x -> "/path"` mounts it. Field lookup and
/// top-level-type lookup are separate tables (`schema::resolve_field`
/// vs. `schema::top_level_type`), so the two roles never collide.
#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    pub name: Ident,
    /// `Some(span)` of the bare `external` flag if it was set; `None`
    /// otherwise — bare-presence only, exactly like [`Network::external`].
    pub external: Option<Span>,
    /// The real underlying Docker volume name, set via the `name` field
    /// (`volume media { name: "media_store" }`), when it differs from
    /// `name.name` above. `None` means "use `name.name` verbatim" —
    /// codegen applies that default, mirroring [`Network::real_name`].
    pub real_name: Option<Literal>,
    /// Compose's own `driver:` key for this volume (`local`, `nfs`, ...).
    pub driver: Option<Literal>,
    /// Compose's own `driver_opts:` map — a free-form `key: value` bag
    /// whose meaning belongs entirely to the chosen driver, so it's
    /// carried through verbatim rather than checked against any schema.
    pub driver_opts: Vec<VolumeDriverOpt>,
    pub span: Span,
}

/// One `key: value` entry inside a top-level `volume`'s `driver_opts`
/// body.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeDriverOpt {
    pub key: Literal,
    pub value: Literal,
    pub span: Span,
}

/// `reference` is `Option` even though `ref` is `image`'s primary field —
/// the parser doesn't enforce required fields (see [`Service`]'s doc), so
/// a syntactically-empty `image {}` must still parse rather than panic.
/// Whether a missing `ref` is acceptable is a semantic/codegen concern.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub reference: Option<Literal>,
    pub span: Span,
}

/// A parsed `build` field (#224) — Compose's own `build:` key, naming a
/// local build context instead of (or beside) a registry `image`.
///
/// Promoted out of `raw` for the reason `healthcheck`/`dns`/`env_file`
/// already were: it's a plain generic Compose key, homelab-specific in
/// none of its own fields. What forced it rather than merely suggesting
/// it is that `build` is the one such key a service can't do without —
/// Compose requires `image` *or* `build`, and before #224 `hllc`
/// required `image` unconditionally, so a locally-built service was not
/// expressible at all, `raw` included (see
/// [`crate::schema::BUILD`] and `hl_codegen`'s own image-or-build
/// check).
///
/// `context` is `Option` for the same reason as [`Image::reference`] —
/// the parser enforces no required fields — but a `build` reaching
/// codegen with no context at all is an error there, since the context
/// is the whole of what there is to build.
///
/// No `args` field this milestone. It's a map, needing the merge
/// machinery `env` has rather than the two plain scalars here, and
/// nothing yet needs one; `raw { build: { ... } }` still overrides the
/// whole key for a service that does.
#[derive(Debug, Clone, PartialEq)]
pub struct Build {
    pub context: Option<Literal>,
    /// A Dockerfile path relative to [`Self::context`], Compose's own
    /// `build.dockerfile`. `None` leaves it to Compose's default
    /// (`Dockerfile` inside the context), and is what decides the
    /// emitted shape: with no `dockerfile`, `build:` serializes as the
    /// bare context string Compose's short form spells, rather than a
    /// one-key mapping saying the same thing.
    pub dockerfile: Option<Literal>,
    pub span: Span,
}

/// A parsed `expose` field. `port` is `Option` for the same reason as
/// [`Image::reference`] — see that doc.
///
/// Through #197, `expose` also modeled exactly one Traefik router of its
/// own (`host`, `entrypoint`). #198 moved every Traefik-routing field
/// onto [`Router`], leaving `port` — Compose's own `expose:` key plus the
/// `loadbalancer.server.port` label, with nothing to do with Traefik —
/// the only thing left here. `expose <port> as "<host>"` still parses,
/// but as bespoke parser sugar that produces an unnamed [`Router`]
/// alongside this `Expose`, not by setting a field of this struct — see
/// `crate::parser::Parser::parse_expose_as_sugar`.
#[derive(Debug, Clone, PartialEq)]
pub struct Expose {
    pub port: Option<Literal>,
    pub span: Span,
}

/// A parsed `router` block (#184) — one Traefik router computed off this
/// service.
///
/// Through #197, [`Expose`] modeled exactly one router of its own — one
/// `host`, one `Host()` rule, one `entrypoints=` label — and a service
/// needing a second router (a public host beside a LAN host, an API path
/// split off from a frontend) had no way to say so short of abandoning
/// `expose` and hand-writing the whole label list in `raw { labels: [...]
/// }`, losing every check `expose` performed. `router` was the second way
/// to say it; #198 then moved every Traefik-routing field off `expose`
/// and onto `router` outright, so today `router` is the *only* way to
/// say it — the unnamed `router { ... }` form (or its `expose <port> as
/// "<host>"` sugar) is simply the common single-router case.
///
/// `name` keys the emitted label: `router api { ... }` on service
/// `vikunja` emits `traefik.http.routers.vikunja-api.*`, while the
/// unnamed `router { ... }` emits `traefik.http.routers.vikunja.*`.
/// Writing the unnamed form twice in one body — by hand, or once by hand
/// and once via `expose <port> as "<host>"`'s own sugar — is a hard
/// error (`ParseError::DuplicateRouterName`): both would claim that same
/// id.
///
/// The name is an [`Ident`], never a string, for the same reason a
/// `service`/`network`/`volume` declaration's name is: it lands in a
/// label *key*, not a value, and `IDENT`'s own grammar
/// (`[A-Za-z_][A-Za-z0-9_-]*`) can't spell the `.`, `=`, backtick, or
/// space a forged `traefik.*` key would need. Codegen re-checks it
/// anyway — see `hl_codegen`'s own router-name guard — so the rule holds
/// for an AST built by any other route too.
///
/// A `router` carries no port. Compose's `loadbalancer.server.port`
/// label is per Compose *service*, not per router, so it stays derived
/// from [`Expose::port`] exactly as before, and several routers off one
/// container all balance onto that one port. `middleware`, unlike the
/// port, genuinely *is* per router, and #221 moved it here off
/// `ServiceFields` outright — see [`Self::middleware`].
#[derive(Debug, Clone, PartialEq)]
pub struct Router {
    /// `None` for the unnamed `router { ... }` form.
    pub name: Option<Ident>,
    /// The hostname this router matches, spliced into the
    /// ``Host(`...`)`` rule. `Option` for the same reason as
    /// [`Image::reference`] — the parser enforces no required fields —
    /// but a router reaching codegen with no host at all is an error
    /// there, since the host is the whole of what creates a router.
    pub host: Option<Literal>,
    /// The Traefik entry points this router attaches to (e.g.
    /// `entrypoints web, web-secure`). A list rather than a scalar because
    /// Traefik's own `entrypoints=` label is itself comma-separated:
    /// modelling that as a list keeps the comma codegen's to write rather
    /// than the user's, so no label value ever has to tolerate one.
    ///
    /// Empty means this router gets no `entrypoints=` label at all —
    /// Traefik's own default of attaching to every entry point — rather
    /// than the parser guessing a homelab-specific value. That's why
    /// this is a plain `Vec` and not an `Option<Vec<_>>`: "unset" and
    /// "set to nothing" have to mean the same thing here, exactly as
    /// they do for [`Self::middleware`] beside it.
    pub entrypoints: Vec<Literal>,
    /// Path prefixes to `&&` onto the `Host()` rule, `||`-joined inside
    /// one parenthesized group: `path_prefix: ["/api/v1", "/dav/"]`
    /// yields ``Host(`h`) && (PathPrefix(`/api/v1`) ||
    /// PathPrefix(`/dav/`))``. `Vec<Literal>`, the same type
    /// `entrypoints` above carries since #196 unified the two — a prefix
    /// is free text a template legitimately parameterizes with `$param`,
    /// which is exactly what that unification made reachable here too
    /// (previously it wasn't, since the pre-#196 `FieldKind::LiteralList`
    /// this field carried existed for that one reason — see
    /// [`crate::schema::allows_qualified_reference`]'s doc for why the
    /// qualified form stays rejected here all the same).
    ///
    /// Order is observable — it's the order the `||` alternatives are
    /// written in — so this concatenates on merge without the
    /// distinct-name dedupe `entrypoints` and `middleware` get, the same
    /// split `dns`/`env_file` already draw against `networks`.
    pub path_prefix: Vec<Literal>,
    /// The Traefik middleware this router attaches (#221) — the whole
    /// of the language's `middleware`, which through #220 was a
    /// service-level field instead.
    ///
    /// Service-level was the wrong scope, not merely a coarser one. A
    /// middleware only ever reaches Traefik as a label on one specific
    /// router, so one list per service could not say what `gitea`
    /// actually needs: a public router beside an internal one, where
    /// only the internal one carries the IP allowlist. A service-wide
    /// list forces the allowlist onto both routers or neither, and both
    /// readings are wrong. Keeping the old field *beside* this one would
    /// have left two spellings of one concept, with precedence rules to
    /// learn and a silent wrong answer whenever someone reached for the
    /// wrong one; #221 moved the field here instead. A body that still
    /// writes the old spelling gets `ParseError::MovedField` rather than
    /// a silently ignored line — see [`crate::schema::moved_field`].
    ///
    /// Empty means this router attaches no middleware, the same way an
    /// empty [`Self::entrypoints`] means it names no entry point: a plain
    /// `Vec`, with "unset" and "set to nothing" deliberately identical.
    pub middleware: Vec<Literal>,
    /// Traefik's own router `priority=` (#225). Higher wins, and it's
    /// the only thing separating two routers that match the same
    /// request — `sftpgo`'s `web` and `webdav` share one host and are
    /// disambiguated purely by this.
    ///
    /// A number, checked as one by `compose::check_numeric_fields`
    /// alongside `expose.port`. `None` leaves the label off entirely,
    /// which is Traefik's own default of deriving a priority from the
    /// rule's length rather than a value `hllc` would be guessing.
    pub priority: Option<Literal>,
    /// The port this router load-balances onto (#225), giving it a
    /// Traefik *service* of its own named after the router's own id.
    ///
    /// `None` falls back to [`ServiceFields::expose`]'s single `port`
    /// and the one service-wide `loadbalancer.server.port` label that
    /// every router shared before #225 — which is what keeps a file
    /// written against the older model emitting exactly what it always
    /// did. `Some` is for the container listening on more than one
    /// port, where "the" port doesn't exist: `sftpgo` serves its web UI
    /// on 2222, WebDAV on 4444, and SFTP on 1111, and each router has
    /// to name which one it means.
    pub port: Option<Literal>,
    /// `http` (the default) or `tcp` (#225) — which of Traefik's two
    /// label namespaces this router lives in.
    ///
    /// A TCP router is not an HTTP router with a flag: it emits
    /// `traefik.tcp.routers.*`/`traefik.tcp.services.*` instead of
    /// `traefik.http.*`, and matches on `HostSNI()` rather than
    /// `Host()`, since at that layer there is no request to read a Host
    /// header from — only the TLS handshake's server name. Raw SFTP
    /// isn't HTTP at all, which is why `sftpgo` needs one.
    ///
    /// Validated in codegen rather than at parse time, so a template can
    /// still write `protocol: $proto`: parameter substitution runs after
    /// parsing, and rejecting an unresolved `$proto` as an unknown
    /// protocol would be a diagnostic about the wrong thing. See
    /// `hl_codegen`'s `CodegenError::UnknownRouterProtocol`.
    pub protocol: Option<Literal>,
    /// The whole Traefik rule, written out (#228) — `rule: Host("a") &&
    /// !PathPrefix("/b")` — instead of assembled from [`Self::host`] and
    /// [`Self::path_prefix`].
    ///
    /// Those two fields could only ever produce one shape: a host match,
    /// optionally `&&`-ed onto a `||`-joined group of path prefixes.
    /// #228 wanted that shape's *inverse* — "this host except these
    /// prefixes" — for a frontend/backend pair splitting one host by
    /// path, and there was no way to say it, so half the pair kept its
    /// whole label list in `raw`.
    ///
    /// The issue asked for a `negate` flag beside `path_prefix`. A flag
    /// buys exactly one more rule: the next router wanting a header
    /// split, a method match, or an `||` at the top level needs a second
    /// flag, and the one after that a third. An expression buys all of
    /// them at once, and buys them in the spelling a user already has in
    /// front of them, since a rule is something copied out of a Traefik
    /// label.
    ///
    /// `host`/`path_prefix` survive as sugar rather than being replaced
    /// by it: the single-host router is the overwhelmingly common case
    /// and deserves its one-liner. Codegen lowers them into this very
    /// type before rendering (`labels::sugar_expr`), so there is one
    /// rule-rendering path rather than two that could disagree. Writing
    /// both spellings on one router is a codegen error — two descriptions
    /// of one rule, with nothing to say which wins.
    pub rule: Option<MatchExpr>,
    pub span: Span,
}

/// One node of a [`Router::rule`] expression (#228) — a boolean
/// expression over Traefik's own rule matchers.
///
/// Matcher arguments are [`Literal`]s, the same type [`Router::path_prefix`]
/// carries, so `$param` substitution and `{{name}}` interpolation reach
/// them through the machinery every other literal slot already goes
/// through, rather than needing a second copy of it.
///
/// [`Self::Group`] is a node rather than something the renderer infers
/// from precedence. Two things pay for it. A rendered rule is then
/// exactly the expression that was written, parentheses included, so the
/// emitted label can be read straight off the source. And it lets the
/// `host`/`path_prefix` sugar lower to `And(Host, Group(prefixes))` and
/// keep emitting the parentheses it always has around a *single*
/// prefix — where they change nothing semantically, and exist so that a
/// rule's shape doesn't depend on how many prefixes it happens to have.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchExpr {
    /// `Host("a")` — a matcher name and its arguments. The name is an
    /// [`Ident`], never a [`Literal`], so it can't be a `$param`: which
    /// matcher this is has to be knowable at parse time, since that's
    /// where the name and its argument count are checked.
    Matcher {
        name: Ident,
        args: Vec<Literal>,
        span: Span,
    },
    /// `!expr`.
    Not { operand: Box<MatchExpr>, span: Span },
    /// `lhs && rhs`.
    And {
        lhs: Box<MatchExpr>,
        rhs: Box<MatchExpr>,
        span: Span,
    },
    /// `lhs || rhs`.
    Or {
        lhs: Box<MatchExpr>,
        rhs: Box<MatchExpr>,
        span: Span,
    },
    /// `( expr )` — parentheses the source actually wrote, kept so the
    /// rendered rule keeps them too.
    Group { inner: Box<MatchExpr>, span: Span },
}

impl MatchExpr {
    /// Where this node was written.
    pub fn span(&self) -> Span {
        match self {
            MatchExpr::Matcher { span, .. }
            | MatchExpr::Not { span, .. }
            | MatchExpr::And { span, .. }
            | MatchExpr::Or { span, .. }
            | MatchExpr::Group { span, .. } => *span,
        }
    }

    /// Every matcher argument in this expression, in source order.
    ///
    /// The one walk composition's passes over a rule share — the
    /// qualified-reference rejection reads them, `$param` substitution
    /// rewrites them through [`Self::args_mut`] — so a node kind added
    /// here can't be visited by one of them and missed by the other.
    pub fn args(&self) -> Vec<&Literal> {
        let mut out = Vec::new();
        self.collect_args(&mut out);
        out
    }

    fn collect_args<'a>(&'a self, out: &mut Vec<&'a Literal>) {
        match self {
            MatchExpr::Matcher { args, .. } => out.extend(args.iter()),
            MatchExpr::Not { operand, .. } | MatchExpr::Group { inner: operand, .. } => {
                operand.collect_args(out)
            }
            MatchExpr::And { lhs, rhs, .. } | MatchExpr::Or { lhs, rhs, .. } => {
                lhs.collect_args(out);
                rhs.collect_args(out);
            }
        }
    }

    /// [`Self::args`], for the passes that rewrite each argument in
    /// place.
    pub fn args_mut(&mut self) -> Vec<&mut Literal> {
        let mut out = Vec::new();
        self.collect_args_mut(&mut out);
        out
    }

    fn collect_args_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Literal>) {
        match self {
            MatchExpr::Matcher { args, .. } => out.extend(args.iter_mut()),
            MatchExpr::Not { operand, .. } | MatchExpr::Group { inner: operand, .. } => {
                operand.collect_args_mut(out)
            }
            MatchExpr::And { lhs, rhs, .. } | MatchExpr::Or { lhs, rhs, .. } => {
                lhs.collect_args_mut(out);
                rhs.collect_args_mut(out);
            }
        }
    }
}

impl Router {
    /// This router's own name, or `None` for the unnamed form — the key
    /// composition merges on and codegen builds the label id from.
    pub fn key(&self) -> Option<&str> {
        self.name.as_ref().map(|n| n.name.as_str())
    }
}

/// A parsed `restart` field. `policy` is `Option` for the same reason as
/// [`Image::reference`] — see that doc.
#[derive(Debug, Clone, PartialEq)]
pub struct Restart {
    pub policy: Option<Literal>,
    pub span: Span,
}

/// A parsed `traefik` field (#159) — the one way to opt a service out of
/// every Traefik label `hl-codegen`'s `labels.rs` otherwise computes for
/// it. See [`crate::schema::TRAEFIK`]'s doc for why this is a `Nested`
/// struct field rather than the bare `traefik disable` spelling the
/// motivating issue first floated.
#[derive(Debug, Clone, PartialEq)]
pub struct Traefik {
    /// `Some(span)` of the bare `disable` flag if it was set; `None`
    /// otherwise — bare-presence only, modeled directly on
    /// [`Network::external`] (see that doc). There is no `disable:
    /// false` form this milestone.
    pub disable: Option<Span>,
    pub span: Span,
}

/// A parsed `healthcheck` field — Compose's own generic `healthcheck:`
/// key (#153). Every sub-field is `Option` for the same reason as
/// [`Image::reference`] (see that doc): the parser doesn't enforce
/// required fields, so `healthcheck {}` must still parse.
///
/// `interval`/`timeout`/`start_period`/`start_interval` are duration
/// strings and `retries` is a number, all carried through as literals
/// exactly as written — see [`crate::schema::HEALTHCHECK`]'s doc for why
/// `hllc` never parses or validates any of them.
#[derive(Debug, Clone, PartialEq)]
pub struct Healthcheck {
    pub test: Option<HealthcheckTest>,
    pub interval: Option<Literal>,
    pub timeout: Option<Literal>,
    pub retries: Option<Literal>,
    pub start_period: Option<Literal>,
    pub start_interval: Option<Literal>,
    /// `Some(span)` of the bare `disable` flag if it was set; `None`
    /// otherwise — bare-presence only, modeled directly on
    /// [`Network::external`] (see that doc). There is no `disable:
    /// false` form this milestone.
    pub disable: Option<Span>,
    pub span: Span,
}

/// `healthcheck`'s `test` sub-field: either a bare literal (Compose's
/// shell form — a bare string is shorthand for `CMD-SHELL <string>`) or
/// a bracketed list of literals (Compose's exec form, `["CMD",
/// "pg_isready", "-U", "miniflux"]`). Carried through to codegen in
/// whichever shape it was written rather than normalizing one into the
/// other (#153) — the two aren't interchangeable: `CMD-SHELL` runs the
/// string through the container's shell, `CMD` execs the list directly
/// with no shell involved.
#[derive(Debug, Clone, PartialEq)]
pub enum HealthcheckTest {
    Shell(Literal),
    /// The list's own span (covering the brackets), since a `Vec` has
    /// nowhere else to carry one.
    Exec(Vec<Literal>, Span),
}

impl HealthcheckTest {
    pub fn span(&self) -> Span {
        match self {
            HealthcheckTest::Shell(lit) => lit.span(),
            HealthcheckTest::Exec(_, span) => *span,
        }
    }
}

/// `command`'s own value (#156): either a bare literal (Compose's shell
/// form — a bare string, `command "npm start"`, is passed to the
/// image's entrypoint as a single shell command) or a bracketed list of
/// literals (Compose's exec form, `command ["npm", "start"]`, passed
/// directly with no shell involved). Structurally identical to
/// [`HealthcheckTest`] — the same shell-vs-exec split Compose itself
/// draws for `healthcheck.test` (#153) — and kept as its own type for
/// the same reason `HealthcheckTest` isn't reused for `test` and
/// `command` both: neither is a natural sub-case of the other, and a
/// shared name would suggest a connection Compose itself doesn't draw
/// between the two keys. Carried through to codegen in whichever shape
/// it was written rather than normalizing one into the other, exactly
/// like `HealthcheckTest` — see that type's own doc.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Shell(Literal),
    /// The list's own span (covering the brackets), since a `Vec` has
    /// nowhere else to carry one.
    Exec(Vec<Literal>, Span),
}

impl Command {
    pub fn span(&self) -> Span {
        match self {
            Command::Shell(lit) => lit.span(),
            Command::Exec(_, span) => *span,
        }
    }
}

/// `entrypoint`'s own value (#183): either a bare literal (Compose's
/// shell form — a bare string, `entrypoint "/bin/sh -c 'do-a-thing'"`,
/// is run through the container's own shell) or a bracketed list of
/// literals (Compose's exec form, `entrypoint ["/bin/sh", "-c",
/// "do-a-thing"]`, run directly with no shell involved). Structurally
/// identical to [`Command`], because Compose's `entrypoint:` key takes
/// exactly the shapes its `command:` key does — and kept as its own
/// type for the same reason [`Command`] isn't reused for `command` and
/// `healthcheck.test` both. Here the separate name earns even more:
/// `entrypoint` and `command` are two *different* Compose keys,
/// overriding the image's `ENTRYPOINT` and its `CMD` respectively, and
/// naming both of them `Command` would suggest they're one setting
/// written two ways. Carried through to codegen in whichever shape it
/// was written rather than normalizing one into the other, exactly like
/// [`Command`].
#[derive(Debug, Clone, PartialEq)]
pub enum Entrypoint {
    Shell(Literal),
    /// The list's own span (covering the brackets), since a `Vec` has
    /// nowhere else to carry one.
    Exec(Vec<Literal>, Span),
}

impl Entrypoint {
    pub fn span(&self) -> Span {
        match self {
            Entrypoint::Shell(lit) => lit.span(),
            Entrypoint::Exec(_, span) => *span,
        }
    }
}

/// The shared entries type behind `volume`, `publish`, and `devices`
/// (#192) — all three are `host -> container` arrow maps with uniqueness
/// on the container side (the value half of `host -> container`),
/// matching Docker's own constraint that two mounts/publications/device
/// mappings can't target the same container-side path even though the
/// same host-side one can appear more than once. One tree type replaces
/// what were three near-identical pairs (`VolumeMap`/`VolumeEntry`,
/// `PublishMap`/`PublishEntry`, `DeviceMap`/`DeviceEntry`) before this
/// type existed — see [`ArrowMapEntry`] for the two ways an entry from
/// one of these three fields can still differ from the other two's.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArrowMap {
    pub entries: Vec<ArrowMapEntry>,
}

/// One `host -> container` entry, shared by `volume`, `publish`, and
/// `devices` (#192). All three fields parse and merge this same shape —
/// see [`crate::schema::VOLUME`]/[`crate::schema::PUBLISH`]/
/// [`crate::schema::DEVICES`] — and differ only in two schema-driven
/// ways, both carried on this one type rather than needing three:
///
/// - [`Self::host`] is an [`ArrowMapHost`], which can only ever be
///   [`ArrowMapHost::Named`] for `volume`
///   ([`crate::schema::TypeSchema::key_may_be_reference`], true for that
///   field alone) — `publish`/`devices` entries are always
///   [`ArrowMapHost::BindMount`].
/// - [`Self::read_only`] (#158) is set only by `volume`'s trailing
///   `{ read_only }` body; Compose's own `devices:`/`ports:` short syntax
///   carries their equivalent suffixes (a cgroup-permissions suffix, a
///   protocol suffix) *inside* the container literal itself instead
///   (`"/dev/xvda:rwm"`, `"53/udp"`), so `publish`/`devices` entries
///   never set this flag — see [`crate::schema::PUBLISH`]/
///   [`crate::schema::DEVICES`] for why that suffix rides the container
///   half rather than becoming a second modifier shape here.
///
/// `read_only` is a plain `bool`, not an `Option<Span>` like
/// [`Network::external`]/[`Volume::external`]: those two report their own
/// span back through [`crate::ParseError::DuplicateField`] when the same
/// struct-kind body sets the flag twice, but an arrow-map entry's body is
/// hand-parsed (see the parser's own `parse_mount_map_entry`)
/// rather than run through the generic struct-field engine, so there is
/// no second occurrence within one entry for a span to ever distinguish—
/// `{ read_only read_only }` is simply a syntax error at the second
/// token, never a value this type has to represent.
///
/// Syntax choice for `read_only` (#158): the issue's own two suggestions
/// were a trailing bare flag after the primary form (`volume "/" ->
/// "/rootfs", read_only`) or a `mode` sub-field body. The trailing-comma
/// form was rejected as genuinely ambiguous, not just unusual: inside
/// `volume`'s existing canonical multi-entry body (`volume { "/" ->
/// "/rootfs", "/var/run" -> "/var/run" }`), a comma already means "the
/// next map entry starts here," and a bare identifier is *already* legal
/// there as the host side of a named-volume entry (`volume { "/" ->
/// "/rootfs", read_only -> "/mnt2" }` legitimately mounts a volume
/// literally named `read_only`). Telling "a flag on the previous entry"
/// from "the start of a new entry that happens to run out of input
/// before its own `->`" apart would need unbounded lookahead the rest of
/// the grammar never asks for, for a distinction the parser can't make
/// locally. A `{ ... }` body sidesteps that entirely: the outer `{` that
/// opens `volume`'s own multi-entry body is only ever checked *before*
/// any entry starts parsing (the parser's own `parse_field`,
/// `SchemaKind::Map` arm), so a second, per-entry `{` after the container
/// literal is never reachable from that position and free of ambiguity.
/// It also mirrors existing precedent directly: `depends_on`'s own
/// per-entry `{ condition: ... }` body (#155) is exactly this shape—
/// `IDENT` (optionally qualified) `->`/`:` value, then an optional `{ }`
/// tail—so `volume "/" -> "/rootfs" { read_only }` reuses a pattern this
/// language's readers already know rather than inventing a second one. A
/// `mode` sub-field (`{ mode: "ro" }`) was rejected on scope grounds
/// alone, not ambiguity: this milestone deliberately covers `:ro` only,
/// not Compose's other short-syntax suffixes (`:z`, `:Z`, tmpfs sizing),
/// and a bare presence flag says exactly that, with nothing left
/// unvalidated the way an arbitrary `mode` string would leave every
/// value but `"ro"` silently unchecked.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrowMapEntry {
    pub host: ArrowMapHost,
    pub container: Literal,
    /// Whether this entry carried a `{ read_only }` body (#158). `false`
    /// for the overwhelming majority of entries, which carry no body at
    /// all—Compose's short syntax omits the `:ro` suffix entirely rather
    /// than spelling out `:rw`, and codegen matches that: this flag adds
    /// a suffix when set and changes nothing about the emitted string
    /// when it isn't (see `hl_codegen`'s `resolve_volumes`). Always
    /// `false` for a `publish`/`devices` entry — see this type's own doc.
    pub read_only: bool,
    pub span: Span,
}

/// The host side of an [`ArrowMapEntry`]: either a path on the machine
/// Compose runs on, or — for a `volume` entry only, see
/// [`ArrowMapEntry`]'s own doc — a reference to a named Docker volume
/// declared by a top-level `volume` declaration.
///
/// Which one is a *syntactic* question, decided here by the parser from
/// the token it read, not later by inspecting the string's shape: a
/// quoted string is always a path (`volume "/mnt/media" -> "/data"`,
/// `volume "./config" -> "/config"`), and a bare identifier is always a
/// reference (`volume syncthing-config -> "/config"`), exactly as a
/// `networks [traefik-net]` entry is. That's what lets a named volume
/// carry an `alias.name` qualifier at all — a string has no structure a
/// qualifier could attach to — and it makes the distinction one the
/// parser enforces rather than one codegen guesses. A `publish`/`devices`
/// entry's host is always [`Self::BindMount`]: neither field's schema
/// sets [`crate::schema::TypeSchema::key_may_be_reference`], so the
/// parser never builds a [`Self::Named`] for either.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrowMapHost {
    /// A quoted path (or any other non-identifier literal, including a
    /// `$param` a template substitutes a path into) bind-mounted from
    /// the host. Needs no declaration — Docker itself requires none for
    /// a host path. The only variant `publish`/`devices` ever produce.
    BindMount(Literal),
    /// A bare `IDENT`, optionally `alias.`-qualified, naming a top-level
    /// [`Volume`] declaration — a [`Literal::Ident`] or
    /// [`Literal::Qualified`], never any other kind (see this type's own
    /// doc for why the parser never builds one from any other token).
    /// Resolved exactly like a `networks [x]` entry: a bare name against
    /// the entry file's own declarations, a qualified one against the
    /// aliased module's. `volume`-only — see this type's own doc.
    Named(Literal),
}

impl ArrowMapHost {
    /// The host's location in source.
    pub fn span(&self) -> Span {
        match self {
            ArrowMapHost::BindMount(lit) | ArrowMapHost::Named(lit) => lit.span(),
        }
    }

    /// The host's text as written: the literal's content for a bind
    /// mount, the referenced declaration's name for a named volume
    /// (without any `alias.` qualifier, which names the file the
    /// declaration lives in rather than the volume — see
    /// [`Literal::text`]).
    pub fn text(&self) -> &str {
        match self {
            ArrowMapHost::BindMount(lit) | ArrowMapHost::Named(lit) => lit.text(),
        }
    }
}

/// `env`'s entries. Uniqueness is checked on `key`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EnvMap {
    pub entries: Vec<EnvEntry>,
}

/// One `key = value` environment entry.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvEntry {
    pub key: Literal,
    pub value: Literal,
    pub span: Span,
}

/// `labels`' entries (#243). Uniqueness is checked on `key`, like
/// [`EnvMap`]'s — the type it is deliberately shaped after, down to the
/// field names, since the two are the same "flat string map with
/// key-side uniqueness" idea applied to two different Compose keys.
///
/// A separate type rather than a reuse of [`EnvMap`] for the reason
/// [`Command`] and [`Entrypoint`] stay separate despite sharing a shape:
/// they are two different Compose keys, and one type standing for both
/// would make every diagnostic and every merge site read as if `env` and
/// `labels` were one field.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabelMap {
    pub entries: Vec<LabelEntry>,
}

/// One `"key": "value"` label entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelEntry {
    pub key: Literal,
    pub value: Literal,
    pub span: Span,
}

/// `raw`'s entries: a schema-free passthrough map with no unknown-field
/// checking and no uniqueness checking — arbitrary keys, values that may
/// themselves recurse into lists or nested maps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawMap {
    pub entries: Vec<RawEntry>,
}

/// One `key: value` entry inside a `raw` body.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEntry {
    pub key: Literal,
    pub value: RawValue,
    pub span: Span,
}

/// A `raw` entry's value: since `raw` is schema-free, its values recurse
/// through the grammar's full `literal | list | nested-map` shape rather
/// than being restricted to a single literal like `volume`/`env`.
#[derive(Debug, Clone, PartialEq)]
pub enum RawValue {
    Literal(Literal),
    List(Vec<RawValue>, Span),
    /// A nested `{ ... }` body under an arbitrary `raw` key.
    Map(Vec<(Literal, RawValue)>, Span),
}

impl RawValue {
    /// The value's location in source.
    pub fn span(&self) -> Span {
        match self {
            RawValue::Literal(lit) => lit.span(),
            RawValue::List(_, span) | RawValue::Map(_, span) => *span,
        }
    }
}

/// One entry in a `with`-list: a template name plus its argument body,
/// e.g. `internal_web { port: 8384 }` or the zero-arg `authenticated`.
/// `args` reuses [`RawMap`] rather than a dedicated type: argument names
/// can't be validated against the target template's declared parameters
/// until composition looks the template up by name in a whole-program
/// symbol table, so at parse time an invocation's argument body is
/// exactly as schema-free as `raw`'s — an arbitrary `key: value` bag
/// whose values may themselves be literals, lists, or nested maps.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateInvocation {
    /// The `alias` in `with alias.name { ... }`, from a `use`-imported
    /// file; `None` for a bare, same-file template name.
    pub qualifier: Option<Ident>,
    pub name: Ident,
    pub args: RawMap,
    pub span: Span,
}

/// The mergeable-fields shape shared by a `service` body and a
/// `template` body (both accept exactly the same field set — see
/// [`TemplateDecl`]'s doc). Factored out so composition's merge
/// operation ("merge N field-bags in priority order") has one signature
/// usable both to resolve a template's own `with`-list and a service's.
///
/// Note: this milestone does not enforce "required" fields (e.g. that a
/// service has an `image`) — that's a semantic/codegen concern, not a
/// syntax one. The parser only enforces "known fields, correct kinds, no
/// illegal duplicates."
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServiceFields {
    pub image: Option<Image>,
    /// Compose's own `build:` key (#224). Independent of
    /// [`Self::image`]: Compose accepts either, or both (build the
    /// context, then tag the result as `image`), and `hl_codegen`
    /// requires only that the emitted service ends up with at least one
    /// of them.
    pub build: Option<Build>,
    pub expose: Option<Expose>,
    /// `router <name> { ... }` blocks (#184), in source order — every
    /// Traefik router this service computes, `expose <port> as
    /// "<host>"`'s own sugared unnamed router included. See [`Router`]'s
    /// own doc.
    ///
    /// Merged by router name, and per sub-field within each name — the
    /// same shape `expose` merges by, so a service body can override
    /// just one sub-field of a template-supplied router of the same
    /// name. See `compose.rs`'s `merge_routers`.
    pub routers: Vec<Router>,
    /// `traefik { disable }` (#159). See [`Traefik`]'s doc. Codegen's
    /// `labels.rs` is the sole reader — a service that leaves this unset
    /// gets exactly today's computed label list, byte for byte.
    pub traefik: Option<Traefik>,
    pub restart: Option<Restart>,
    /// Compose's own `healthcheck:` key (#153). See [`Healthcheck`]'s
    /// doc.
    pub healthcheck: Option<Healthcheck>,
    /// `publish 8096 -> 8096` entries — Compose's `ports:` key. Distinct
    /// from `expose` above, which is Compose's `expose:` (visible to
    /// other containers on the same network, never published to the
    /// host) plus the `loadbalancer.server.port` label. Shares [`ArrowMap`] with
    /// [`Self::volumes`]/[`Self::devices`] below — see that type's own
    /// doc for the shape all three fields share and the two ways
    /// `volume` alone still differs.
    pub publish: ArrowMap,
    pub volumes: ArrowMap,
    pub env: EnvMap,
    /// `labels { "com.example.owner": "platform-team" }` (#243) — extra
    /// Docker labels, **added** to the set `hl_codegen`'s `labels.rs`
    /// computes from `router`/`expose`/`traefik`/the docker-network
    /// label rather than replacing it, which is the one thing
    /// `raw { labels: [...] }` cannot do (#232).
    ///
    /// Emitted after every computed label, so a service that writes none
    /// of these gets exactly the label list it got before this field
    /// existed, byte for byte.
    ///
    /// A key that collides with a computed one is a hard error in
    /// codegen (`CodegenError::LabelCollidesWithGenerated`), not a
    /// precedence rule: whichever side won silently, one of the two
    /// lines the author wrote would do nothing, which is the failure
    /// #193, #206 and #232 all exist to close.
    ///
    /// Merged across template tiers exactly like [`Self::env`] — same
    /// [`crate::schema::MapSide::Key`] convention, same `merge_map`, so
    /// own wins over a template, `defaults` always loses, and two
    /// explicit templates setting one key collide.
    pub labels: LabelMap,
    pub raw: RawMap,
    /// `depends_on [db]` / `depends_on [db { condition: service_healthy }]`
    /// (#155) — each entry is a same-file service reference, optionally
    /// carrying an explicit Compose readiness condition. Unlike
    /// `networks`/`dns`/`env_file`, this isn't a plain
    /// [`Literal`] list: a bare reference has nowhere to hang the
    /// optional `{ condition: ... }` body, so it's its own
    /// [`DependsOnEntry`] type instead — see that type's own doc for why
    /// it isn't shaped as a [`TemplateInvocation`] either, despite the
    /// surface syntax looking similar. Still accumulates across repeated
    /// `depends_on` statements and gets the bare single-item sugar every
    /// list field gets, exactly as it always has.
    ///
    /// Composition merges this field keyed on the referenced service's
    /// own name, via `compose.rs`'s dedicated `merge_depends_on` rather
    /// than the plain set-like distinct-name dedupe every other
    /// reference-list field goes through — but a plain, all-bare
    /// `depends_on` behaves exactly as it always has: two templates each
    /// naming the same dependency with no condition (or with the same
    /// one) still silently collapse to one entry, since they say the
    /// same thing twice, not two different things. Only two *explicit*
    /// templates naming the same service with *differing* conditions
    /// have anything to disagree about, so only that case raises a
    /// [`crate::compose::ComposeError::MapKeyCollision`] — see
    /// `compose.rs`'s `merge_depends_on` for the exact rule.
    pub depends_on: Vec<DependsOnEntry>,
    /// `networks [proxy, alias.other]` — the one reference-list field
    /// whose qualified form composition actually resolves, rather than
    /// rejecting: see [`crate::schema::allows_qualified_reference`].
    /// `$param` is legal here since #196 unified this field's entries
    /// onto [`Literal`] — `template web(net: String) { networks [$net] }`
    /// — a network name is exactly the kind of value a template
    /// legitimately parameterizes, and composition's own
    /// `resolve_networks`-style by-name lookup at codegen still catches a
    /// substituted `$net` that names nothing declared.
    pub networks: Vec<Literal>,
    /// A per-service DNS resolver override (Compose's own `dns:` key,
    /// e.g. a LAN resolver IP) — a plain generic Compose key like
    /// `volume`/`env`/`expose`, not homelab-specific itself even though
    /// any one entry's value always is. List-typed and reference-list
    /// shaped like `depends_on`/`networks` (accumulates
    /// across repeats, never duplicate-checked), even though its entries
    /// are ordinary literal values (IP addresses) rather than references
    /// to another declaration — reusing [`Literal`] costs nothing here:
    /// an entry can syntactically carry an `alias.` qualifier like any
    /// other reference-shaped position, but
    /// `compose::reject_qualified` rejects it, since a DNS
    /// server address is never something an `.hll` file declares for a
    /// qualifier to resolve against.
    pub dns: Vec<Literal>,
    /// `env_file "one.env"` / `env_file ["one.env", "two.env"]` — paths
    /// to load environment variables from, Compose's own `env_file:`
    /// key (#154). Same reasoning as [`Self::dns`] just above: a plain
    /// generic Compose key, not homelab-specific itself even though most
    /// real entries point at a gitignored, homelab-specific `.env` file,
    /// list-typed and reference-list shaped like
    /// `depends_on`/`networks`/`dns` (accumulates across
    /// repeats, never duplicate-checked, and a bare `env_file "one.env"`
    /// is sugar for a one-element list), even though its entries are
    /// ordinary path strings rather than references to another
    /// declaration. Reusing [`Literal`] costs nothing here for the same
    /// reason it costs nothing for `dns`: Compose's paths are resolved
    /// relative to the compose file, which is the user's concern, not
    /// `hllc`'s, so a path is carried through verbatim either way and a
    /// qualifier — syntactically legal, semantically rejected, exactly
    /// like `dns`'s — never resolves to anything.
    pub env_file: Vec<Literal>,
    /// `privileged` — Compose's own `privileged:` key (#157), giving the
    /// container extended host privileges (`cadvisor`'s classic use
    /// case: reading host `/proc`/cgroups). A plain generic Compose key
    /// like `dns`/`env_file`, not homelab-specific itself. Modeled
    /// directly on [`Network::external`]: a bare-presence flag with no
    /// `: value` form — there is no `privileged: false` to write, since
    /// absence already means false, so a colon after it is a parse
    /// error rather than an attempted value.
    pub privileged: Option<Span>,
    /// `devices "/dev/kmsg" -> "/dev/kmsg"` — host device paths mapped
    /// onto container device paths, Compose's own `devices:` key (#157).
    /// A plain generic Compose key, not homelab-specific itself even
    /// though a real entry always is — but map-kind rather than
    /// reference-list shaped, mirroring [`Self::publish`] field for
    /// field rather than `dns`/`env_file`: #167's review feedback asked
    /// for the same `host -> container` arrow spelling `publish`/
    /// `volume` already use in place of the original pre-joined
    /// `"host:container"` string, so this now merges key-by-key on the
    /// container side through `compose.rs`'s `merge_map`, exactly like
    /// `publish`, rather than concatenating through `LIST_FIELDS`. See
    /// [`crate::schema::DEVICES`] for the uniqueness-side reasoning.
    pub devices: ArrowMap,
    /// Docker's own `container_name:` key. `None` means "default to the
    /// service's own name" (via the same `{{name}}` interpolation
    /// binding `expose`'s `as`-sugar already uses) — codegen, not the
    /// parser or composition, applies that default, mirroring how
    /// [`Network::real_name`] staying `Option` defers its own default to
    /// a later stage.
    pub container_name: Option<Literal>,
    /// Compose's own generic `command:` key (#156), overriding the
    /// image's entrypoint arguments. A plain scalar-or-list field
    /// directly on `service`/`template`, like [`Self::container_name`]
    /// just above, rather than a nested struct type — it has no
    /// secondary fields of its own. See [`Command`]'s own doc for the
    /// shell-vs-exec shape it carries, structurally identical to
    /// [`Healthcheck::test`]'s [`HealthcheckTest`] (#153) — `command` is
    /// modeled on that field rather than on `dns`/`env_file`'s plain
    /// [`Literal`] lists, since Compose's `command:` key, like
    /// `healthcheck.test`, is either one bare string or a bracketed
    /// list, never a bare comma-separated sequence.
    pub command: Option<Command>,
    /// Compose's own generic `entrypoint:` key (#183), overriding the
    /// image's own `ENTRYPOINT`. A separate Compose key from
    /// [`Self::command`] just above, which overrides the image's `CMD`
    /// instead — the two are set independently, and a service may set
    /// either, both, or neither. Shaped exactly like [`Self::command`]
    /// otherwise: a plain scalar-or-list field directly on
    /// `service`/`template` with no secondary fields of its own. See
    /// [`Entrypoint`]'s own doc for the shell-vs-exec shape it carries.
    pub entrypoint: Option<Entrypoint>,
    /// Unresolved template invocations pulled in via `with`. Always
    /// empty after [`crate::compose::compose`] runs — composition's job
    /// is precisely to merge each of these away.
    pub with: Vec<TemplateInvocation>,
}

/// A parsed `service` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: Ident,
    pub fields: ServiceFields,
    pub span: Span,
}

/// A parsed `template` declaration: a named, optionally parameterized
/// block producing a *partial* [`ServiceFields`] record, meant to be
/// merged onto a real `service` (or another template) via `with`. A
/// template's body accepts exactly the same fields as a `service` body —
/// see docs/DESIGN.md's Composition section.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDecl {
    pub name: Ident,
    /// Declared parameters, in source order. Empty for a zero-parameter
    /// template (`template foo { ... }` or `template foo() { ... }` —
    /// both parse to the same empty `Vec`).
    pub params: Vec<Param>,
    pub fields: ServiceFields,
    pub span: Span,
}
