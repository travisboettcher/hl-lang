use hl_lexer::TokenKind;

/// Whether a type's body is a fixed set of named fields (`struct`) or an
/// arbitrary key→value collection (`map`). See docs/DESIGN.md's Grammar
/// section for the full rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Struct,
    Map,
}

/// Which side of a map entry gets uniqueness-checked. `env` checks the
/// key (two entries can't both claim the same name); `volume` checks the
/// value/container-path (Docker itself refuses two mounts at the same
/// container path, but allows the same host path mounted twice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapSide {
    Key,
    Value,
}

/// How a single struct field may be set.
#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    /// A single literal value. Writing it twice in one body is a
    /// [`crate::ParseError::DuplicateField`].
    Scalar,
    /// A boolean flag settable only by bare presence (`external`,
    /// `healthcheck.disable`, `privileged` — see #157), with no
    /// `: value` form this milestone — its span is recorded when set.
    BoolFlag,
    /// The field's value is itself an instance of another registered
    /// type. Struct-kind nested types are single-occurrence (a second
    /// write is `DuplicateField`); map-kind nested types accumulate
    /// entries across repeated writes (per docs/DESIGN.md's rule 4).
    Nested(&'static TypeSchema),
    /// A list of reference-shaped [`crate::ast::Literal`]s — `middleware`,
    /// `networks`, `dns`, `env_file`, `router.entrypoints`, and
    /// `router.path_prefix`. Accumulates across
    /// repeats; settable via a bracketed list, the bare comma-list sugar,
    /// or repeated statements — never duplicate-checked, since list
    /// fields can't collide.
    ///
    /// Absorbed what used to be the separate `LiteralList` kind (#196):
    /// before #196's [`crate::ast::Literal`]/`Reference` unification,
    /// `router`'s `path_prefix` needed its own kind purely because a
    /// `Reference` couldn't carry a `$param` and a `Literal` couldn't
    /// carry a qualifier, so a field that had to accept `$param` (every
    /// realistic `path_prefix`) couldn't be parsed the same way as one
    /// that had to accept a qualifier (`middleware`/`networks`/...). Now
    /// that one type does both, the two kinds parse and store identically
    /// and there is nothing left to distinguish — see
    /// [`allows_qualified_reference`] for the one way individual rows
    /// still differ (whether a qualifier is *semantically* legal, not
    /// syntactically), and `compose::reject_qualified` for where
    /// that's enforced.
    ///
    /// `devices` moved off this kind and onto [`SchemaKind::Map`] (see
    /// [`DEVICES`]) once #167's review feedback asked for the same
    /// `"host" -> "container"` arrow spelling `publish`/`volume` already
    /// use, rather than a pre-joined `"host:container"` string.
    ///
    /// `depends_on` moved off this kind and onto its own
    /// [`Self::DependsOnList`] when its entries gained an optional
    /// `{ condition: ... }` body (#155) — a bare reference has nowhere to
    /// hang that.
    ReferenceList,
    /// A list of template invocations (`with`'s `templates` field): each
    /// item is an `IDENT` naming a template, optionally followed by a
    /// `{ arg: value, ... }` argument body. Parses like [`Self::ReferenceList`]
    /// (bracketed list, bare comma-list sugar, accumulates, never
    /// duplicate-checked) except each item can carry an argument body.
    TemplateInvocationList,
    /// Either a single literal or a bracketed list of literals —
    /// `healthcheck`'s `test` (#153), `command` (#156), and
    /// `entrypoint` (#183), which each
    /// carry Compose's own matching pair of shapes: a bare string (shell
    /// form, `test: "curl -f http://localhost"` /
    /// `command: "npm start"`) or a list (exec form, `test: ["CMD",
    /// "curl", "-f", "http://localhost"]` /
    /// `command: ["npm", "start"]`). Single-occurrence like
    /// [`Self::Scalar`] (a second write is `DuplicateField`) — unlike
    /// [`Self::ReferenceList`], there is no bare comma-list sugar here,
    /// since `test: "CMD", "curl"` would be ambiguous between "the shell
    /// string followed by garbage" and "a two-item exec list"; Compose's
    /// own two forms are told apart by brackets alone, so `hll` requires
    /// the same — a bare literal or an explicit `[...]`, nothing in
    /// between.
    ScalarOrList,
    /// `depends_on`'s own list kind (#155): like [`Self::ReferenceList`]
    /// — accumulates across repeats, settable via a bracketed list, the
    /// bare comma-list sugar, or repeated statements, never
    /// duplicate-checked at parse time — except each entry may also
    /// carry an optional `{ condition: ... }` body, matching
    /// [`Self::TemplateInvocationList`]'s own "`IDENT` optionally
    /// followed by a `{ }` body" shape. Not literally
    /// `TemplateInvocationList`, though, because that body isn't
    /// schema-free the way a template invocation's argument bag is:
    /// `condition` is the one and only legal key, and its value must be
    /// one of Compose's own three fixed keywords
    /// (`service_started`/`service_healthy`/
    /// `service_completed_successfully`), checked immediately by the
    /// parser rather than deferred — see
    /// [`crate::ast::DependsOnEntry`]'s doc and
    /// [`crate::ParseError::InvalidDependsOnCondition`].
    DependsOnList,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldSchema {
    pub name: &'static str,
    pub kind: FieldKind,
}

/// The schema for one type name (`service`, `image`, `volume`, ...). This
/// is the mechanism that keeps the parser a single generic engine instead
/// of one function per keyword — see docs/DESIGN.md's Pipeline section.
#[derive(Debug, Clone, Copy)]
pub struct TypeSchema {
    pub type_name: &'static str,
    pub kind: SchemaKind,
    /// Empty for `Map`-kind types (their entries aren't a fixed field
    /// set).
    pub fields: &'static [FieldSchema],
    /// The one field a bare value right after the type name sets
    /// (docs/DESIGN.md's desugaring rule 1). `None` for map-kind types
    /// (their "primary" shorthand is the bare-entry sugar instead, via
    /// `map_separator`).
    pub primary_field: Option<&'static str>,
    /// The bare-entry separator token for map-kind types (`->` for
    /// `volume`, `=` for `env`, `:` for `raw` — `raw`'s separator being
    /// literally `:` means it needs no extra sugar path at all, since
    /// that's already the canonical form).
    pub map_separator: Option<TokenKind>,
    /// Which side of a map entry is uniqueness-checked. Meaningful only
    /// for a [`SchemaKind::Map`] type — every one of those defines a
    /// side, `raw` included as of #193 — and always `None` for a
    /// [`SchemaKind::Struct`] type, which has no map entries for a side
    /// to describe.
    pub uniqueness: Option<MapSide>,
    /// Map-kind types only: whether a bare `IDENT` on the *key* side of
    /// an entry is a reference to a top-level declaration
    /// ([`crate::ast::ArrowMapHost::Named`]) rather than an ordinary
    /// literal — which also lets it carry an `alias.` qualifier.
    ///
    /// True for [`VOLUME`] alone. `env`/`publish`/`driver_opts`/`raw`
    /// keys are plain literal values with nothing to resolve against, so
    /// they keep the ordinary all-literal entry parsing.
    pub key_may_be_reference: bool,
    /// Whether an instance needs an instance name (`network foo { ... }`,
    /// `service foo { ... }`) — true only for the two top-level types.
    pub needs_name: bool,
    /// `raw` only: unknown keys are accepted rather than rejected, and
    /// values recurse generically instead of being checked against a
    /// fixed field list.
    pub schema_free: bool,
}

/// `image "ref" ` / `image { ref: "..." }`.
pub static IMAGE: TypeSchema = TypeSchema {
    type_name: "image",
    kind: SchemaKind::Struct,
    fields: &[FieldSchema {
        name: "ref",
        kind: FieldKind::Scalar,
    }],
    primary_field: Some("ref"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `build "./vault-git-sync"` / `build { context: "...", dockerfile:
/// "..." }` — Compose's own `build:` key (#224).
///
/// `context` is the primary field, for [`IMAGE`]'s exact reason: one
/// bare value stands in for the whole struct, since a build with a
/// context and nothing else is the overwhelmingly common case and
/// Compose has its own short form (`build: ./dir`) saying precisely
/// that. `dockerfile` is the one other plain scalar Compose's long form
/// takes that a service realistically needs; `args` is deliberately
/// absent — see [`crate::ast::Build`]'s doc.
pub static BUILD: TypeSchema = TypeSchema {
    type_name: "build",
    kind: SchemaKind::Struct,
    fields: &[
        FieldSchema {
            name: "context",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "dockerfile",
            kind: FieldKind::Scalar,
        },
    ],
    primary_field: Some("context"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `expose 8096` / `expose { port: 8096 }` — Compose's own `expose:`
/// key, which declares a port visible to other containers on the network
/// without publishing it to the host.
///
/// Nothing else. Through #271 this field also fed
/// `traefik.http.services.<svc>.loadbalancer.server.port`, and before
/// #198 it modelled a whole Traefik router of its own (`host`,
/// `entrypoint`). Both are gone — routing is `std:traefik`'s — and what
/// is left is the plain Compose concern the keyword names.
///
/// `expose <port> as "<host>"` went with them. It was the shortest
/// spelling of the common single-router service and desugared to an
/// unnamed `router { host }`, a thing this language no longer has. The
/// parser still recognizes the `as` to say so rather than failing on a
/// stray token — see [`crate::ParseError::RemovedExposeAsSugar`], raised
/// where the sugar used to be accepted.
pub static EXPOSE: TypeSchema = TypeSchema {
    type_name: "expose",
    kind: SchemaKind::Struct,
    fields: &[FieldSchema {
        name: "port",
        kind: FieldKind::Scalar,
    }],
    primary_field: Some("port"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

pub static RESTART: TypeSchema = TypeSchema {
    type_name: "restart",
    kind: SchemaKind::Struct,
    fields: &[FieldSchema {
        name: "policy",
        kind: FieldKind::Scalar,
    }],
    primary_field: Some("policy"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `healthcheck { test: "...", interval: "10s", ... }` — Compose's own
/// generic `healthcheck:` key (#153). Every field here
/// (`test`/`interval`/`timeout`/`retries`/`start_period`/
/// `start_interval`/`disable`) is a plain Compose key, not
/// homelab-specific in any of its own fields — the same "generic core"
/// reasoning that already justified [`NETWORK`]'s `external` and the
/// reference-list fields on `SERVICE_FIELDS` (`dns`/`env_file`).
///
/// No `primary_field`, unlike [`IMAGE`]'s `ref` or [`EXPOSE`]'s `port`:
/// there's no one sub-field an unadorned `healthcheck "..."` could
/// obviously mean. `test` alone doesn't stand in for the whole
/// healthcheck the way a single reference stands in for `image` — a
/// realistic healthcheck sets `test` alongside `interval`/`timeout`/
/// `retries` too, so the braced body (`healthcheck { ... }`) is
/// required.
///
/// `interval`/`timeout`/`start_period`/`start_interval` are duration
/// strings and `retries` is a number, all `FieldKind::Scalar` and
/// carried through as literals exactly as written — `hllc` does not
/// parse or validate Compose's duration syntax (`"10s"`, `"1m30s"`) or
/// check that `retries` is non-negative; that's Compose's job at deploy
/// time, not the compiler's at compile time.
///
/// `disable` is modeled directly on [`NETWORK`]'s `external`: a
/// bare-presence [`FieldKind::BoolFlag`], matching Compose's own
/// `disable: true` — there is no `disable: false` form this milestone,
/// same reasoning as `external`. Compose's `disable: true` turns the
/// healthcheck off entirely, including one inherited from the image.
pub static HEALTHCHECK: TypeSchema = TypeSchema {
    type_name: "healthcheck",
    kind: SchemaKind::Struct,
    fields: &[
        FieldSchema {
            name: "test",
            kind: FieldKind::ScalarOrList,
        },
        FieldSchema {
            name: "interval",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "timeout",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "retries",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "start_period",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "start_interval",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "disable",
            kind: FieldKind::BoolFlag,
        },
    ],
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `volume`, `publish`, and `devices` (#192) are three schema rows over
/// one shared shape: a `->`-separated `host -> container` arrow map,
/// uniqueness on the container (value) side, parsed and merged into one
/// [`crate::ast::ArrowMap`] of [`crate::ast::ArrowMapEntry`]s — see that
/// type's own doc for the full rationale, including the two ways `volume`
/// alone still differs from its two siblings. Each `static` below is only
/// what actually varies row to row.
///
/// The `volume` *field* on a `service`/`template`: `volume "/host/path"
/// -> "/container"` / `volume { "/host/path": "/container" }` for a bind
/// mount, `volume named-volume -> "/container"` for a named one.
///
/// The one map-kind type with [`TypeSchema::key_may_be_reference`] set:
/// a bare `IDENT` on the host side is a reference to a top-level
/// `volume` declaration rather than a literal, so it can be
/// `alias.`-qualified like any other cross-file reference. See
/// [`crate::ast::ArrowMapHost`].
///
/// Not to be confused with [`VOLUME_DECL`], the top-level `volume name
/// { ... }` declaration this field's named-volume entries resolve
/// against. The two share an identifier but never a lookup table: a
/// field name is resolved through [`resolve_field`] against the
/// enclosing type's own field list, a top-level type name through
/// [`top_level_type`], and neither ever consults the other.
pub static VOLUME: TypeSchema = TypeSchema {
    type_name: "volume",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
    key_may_be_reference: true,
    needs_name: false,
    schema_free: false,
};

/// `publish 8096 -> 8096` / `publish { 8096: 8096 }` — a host-port →
/// container-port mapping, emitted as Compose's `ports:` list. See
/// [`VOLUME`]'s own doc for the shape all three arrow-map rows share.
/// Kept entirely separate from [`EXPOSE`], which keeps its own meaning
/// (Compose's `expose:` — container-network visibility only, plus the
/// Traefik router labels) unchanged.
///
/// Uniqueness lands on the container port rather than the host one:
/// Docker's real conflict is on the host side, but a protocol suffix
/// rides on the container half of a Compose short-syntax mapping
/// (`53:53/udp`), so a host-side check would reject the very
/// configuration this field exists to make expressible — Pi-hole
/// publishing both `53 -> "53/tcp"` and `53 -> "53/udp"`. Checking the
/// container side still catches the copy-paste case (the same target
/// port written twice) and leaves the legitimate one alone.
pub static PUBLISH: TypeSchema = TypeSchema {
    type_name: "publish",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `devices "/dev/kmsg" -> "/dev/kmsg"` / `devices { "/dev/kmsg":
/// "/dev/kmsg" }` — a host device path → container device path mapping,
/// emitted as Compose's `devices:` list. See [`VOLUME`]'s own doc for the
/// shape all three arrow-map rows share. Grew this arrow spelling in
/// place of a pre-joined `"host:container"` string per review feedback on
/// #167, after #157 originally shipped it as a [`FieldKind::ReferenceList`].
///
/// Uniqueness on the container side — exactly [`PUBLISH`]'s own
/// reasoning, and it transfers over unchanged. Docker's real conflict on
/// a `devices` short-syntax entry is on the host side, but Compose's
/// short syntax is `HOST:CONTAINER[:CGROUP_PERMISSIONS]`, so an optional
/// `rwm`-style permissions suffix rides the *container* half
/// (`"/dev/sda" -> "/dev/xvda:rwm"`) — the direct analogue of `publish`'s
/// protocol suffix riding its own container half (`53 -> "53/udp"`). A
/// host-side uniqueness check would reject the legitimate case this
/// makes expressible: the same host device mapped to two different
/// container paths, each with its own permissions. Checking the
/// container side still catches the copy-paste case (the same target
/// path written twice) and leaves the legitimate one alone.
pub static DEVICES: TypeSchema = TypeSchema {
    type_name: "devices",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `env KEY = "value"` / `env { KEY: "value" }`. Uniqueness on the key
/// side.
pub static ENV: TypeSchema = TypeSchema {
    type_name: "env",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Equals),
    uniqueness: Some(MapSide::Key),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `labels { "com.example.owner": "platform-team" }` — extra Docker
/// labels written by hand, *added* to the label set `router`/`expose`/
/// `traefik`/the docker-network label already compute for the service
/// (#243). Map-kind with a `:` separator, so — like `raw` and
/// `driver_opts` — its bare-entry and canonical forms are the same
/// thing, and key-side uniqueness, like [`ENV`].
///
/// **Map-shaped rather than a list of `"k=v"` strings.** A list would
/// read closer to Traefik's own documentation and would need no quoting
/// around a dotted key, but it would also be the one collection in the
/// language with no uniqueness side, which is precisely the silent-loss
/// hazard #193 and #206 closed everywhere else. A map reuses
/// [`TypeSchema::uniqueness`] as it stands, so a key repeated inside one
/// body is a [`crate::ParseError::DuplicateMapKey`] naming both spans,
/// exactly as `env`/`volume`/`publish`/`driver_opts` already behave.
/// Compose's own `labels:` key takes a map form natively too, so this is
/// also the spelling that matches Compose.
///
/// Not `schema_free`: a label value is a flat string, never a nested
/// YAML tree, so [`RAW`]'s recursing [`crate::ast::RawValue`] would buy
/// nothing here and would let a `labels` entry describe something a
/// Docker label cannot hold.
///
/// A key that collides with a *generated* label is a codegen error
/// rather than a parse error — see
/// [`crate::ast::ServiceFields::labels`] and `hl_codegen`'s
/// `labels::compute`, which is the only place that knows what the other
/// features computed.
pub static LABELS: TypeSchema = TypeSchema {
    type_name: "labels",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Colon),
    uniqueness: Some(MapSide::Key),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// `raw { any_key: any_value }` — schema-free passthrough, no unknown-key
/// checking. `uniqueness` is `Some(MapSide::Key)`, the same convention
/// `env` uses, since #193: a `with`-merge collision on a repeated `raw`
/// key now raises the same `MapKeyCollision` a repeated `env` key does,
/// rather than silently keeping whichever tier merged last.
///
/// Both tiers read this field as of #206. The cross-tier merge in
/// `hl_parser::compose` was the first; the parser's own `schema_free`
/// body-parsing path is the second, so a `raw` key repeated within *one*
/// body is a `DuplicateMapKey` too, exactly as a repeated `env` key
/// already was. Only the key side is ever meaningful here — `raw`'s
/// values are `RawValue` trees rather than literals, so there is nothing
/// on the value side to compare.
pub static RAW: TypeSchema = TypeSchema {
    type_name: "raw",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Colon),
    uniqueness: Some(MapSide::Key),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: true,
};

/// A top-level `network name { ... }` declaration. `name` (the field, not
/// the declaration's own identifier) is the real underlying Docker
/// network name, when it differs from the declaration's own identifier —
/// e.g. `network traefik-net { external, name: "docker_default" }`,
/// needed because Compose's own auto-derived network names are specific
/// to one homelab's directory layout and can't be assumed by the
/// compiler (see [`crate::ast::Network::real_name`]).
pub static NETWORK: TypeSchema = TypeSchema {
    type_name: "network",
    kind: SchemaKind::Struct,
    fields: &[
        FieldSchema {
            name: "external",
            kind: FieldKind::BoolFlag,
        },
        FieldSchema {
            name: "name",
            kind: FieldKind::Scalar,
        },
    ],
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: true,
    schema_free: false,
};

/// A top-level `volume` declaration's `driver_opts { key: value }` body
/// — Compose's own free-form per-driver option bag. Map-kind with a `:`
/// separator (so, like `raw`, its bare-entry and canonical forms are the
/// same thing) and key-side uniqueness, like `env`: two entries can't
/// both claim the same option name. Unlike `raw` it is *not*
/// `schema_free`, because the values are plain literals rather than
/// arbitrarily nested YAML — Compose's `driver_opts` is a flat
/// string→string map.
pub static DRIVER_OPTS: TypeSchema = TypeSchema {
    type_name: "driver_opts",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Colon),
    uniqueness: Some(MapSide::Key),
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// A top-level `volume name { ... }` declaration — the declaration a
/// service's named-volume mount (`volume syncthing-config -> "/config"`)
/// has to resolve against, exactly as a `networks [x]` entry resolves
/// against [`NETWORK`]. `external` and `name` mean precisely what they
/// mean on a `network` (see [`NETWORK`]'s doc and
/// [`crate::ast::Volume::real_name`]); `driver`/`driver_opts` are the
/// two extra Compose knobs that exist only on the volume side.
///
/// This shares its `type_name` with [`VOLUME`], the service-level
/// `volume` *field*, on purpose: to a user they are one concept
/// (`volume`) written in two positions, and an `UnknownField` on either
/// should say "volume". See [`VOLUME`]'s doc for why the shared name
/// can't cause a resolution collision.
pub static VOLUME_DECL: TypeSchema = TypeSchema {
    type_name: "volume",
    kind: SchemaKind::Struct,
    fields: &[
        FieldSchema {
            name: "external",
            kind: FieldKind::BoolFlag,
        },
        FieldSchema {
            name: "name",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "driver",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "driver_opts",
            kind: FieldKind::Nested(&DRIVER_OPTS),
        },
    ],
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: true,
    schema_free: false,
};

/// `with template_name { arg: value, ... }, other_template, ...` — a
/// template-invocation list nested one level under the `with` field
/// itself, matching docs/DESIGN.md's schema table row for `with`
/// literally (`struct` kind, primary field `templates`).
pub static WITH: TypeSchema = TypeSchema {
    type_name: "with",
    kind: SchemaKind::Struct,
    fields: &[FieldSchema {
        name: "templates",
        kind: FieldKind::TemplateInvocationList,
    }],
    primary_field: Some("templates"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: false,
    schema_free: false,
};

/// The field set shared by a `service` body and a `template` body — per
/// docs/DESIGN.md, a template is "a named, optionally parameterized
/// block that produces a *partial* record of fields, meant to be merged
/// onto a real `service`," so the two bodies accept exactly the same
/// fields. Factored out once so `SERVICE` and `TEMPLATE` can't drift
/// apart.
static SERVICE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "image",
        kind: FieldKind::Nested(&IMAGE),
    },
    // `container_name "uptime-kuma"` / `container_name: "uptime-kuma"`.
    // A plain scalar field directly on `service`/`template` (not a
    // nested struct type like `image`/`expose`/`restart` — it's a
    // single Compose value with no secondary fields of its own).
    // Unset means "default to the service's own name" per
    // docs/DESIGN.md's Composition section — the same deferred-default
    // pattern as `ast::Network::real_name`, applied at codegen time
    // rather than here or during composition.
    FieldSchema {
        name: "container_name",
        kind: FieldKind::Scalar,
    },
    // `command "npm start"` (shell form) / `command ["npm", "start"]`
    // (exec form) — Compose's own generic `command:` key (#156),
    // overriding the image's entrypoint arguments. A plain scalar-or-list
    // field directly on `service`/`template`, not a nested struct type —
    // it has no secondary fields of its own, so it needs
    // `FieldKind::ScalarOrList` (see that variant's doc) rather than
    // `Scalar` alone. Modeled directly on `healthcheck`'s `test`
    // sub-field (#153), the only other field with this exact shape: both
    // carry Compose's own shell-vs-exec distinction, carried through
    // verbatim rather than normalized one into the other. See
    // `ast::ServiceFields::command`'s doc for why `command` sits directly
    // on `ServiceFields` instead of inside a nested struct the way
    // `test` sits inside `healthcheck`.
    FieldSchema {
        name: "command",
        kind: FieldKind::ScalarOrList,
    },
    // `entrypoint "/bin/sh -c 'do-a-thing'"` (shell form) /
    // `entrypoint ["/bin/sh", "-c", "do-a-thing"]` (exec form) —
    // Compose's own generic `entrypoint:` key (#183), overriding the
    // image's `ENTRYPOINT` where `command` just above overrides its
    // `CMD`. Same `FieldKind::ScalarOrList` shape as `command` for the
    // same reason: Compose gives both keys the identical
    // shell-string-or-exec-list pair of forms.
    //
    // Through #198 this name was shared with a `router`'s own
    // entry-point list, an unrelated reference list of Traefik names —
    // the same two-roles-one-identifier situation `volume` is still in
    // (see [`top_level_type`]'s doc). It was never ambiguous to the
    // parser, since a field name is only ever resolved through
    // [`resolve_field`] against the enclosing type's own field list, but
    // it was ambiguous to a reader with only position to go on. #199
    // renamed the router's field to `entrypoints`; this row, Compose's
    // own key, keeps the name Compose gives it.
    FieldSchema {
        name: "entrypoint",
        kind: FieldKind::ScalarOrList,
    },
    // `build` sits beside `image` because the two answer one question
    // between them — where this service's container image comes from —
    // and `hl_codegen` checks them together (#224).
    FieldSchema {
        name: "build",
        kind: FieldKind::Nested(&BUILD),
    },
    FieldSchema {
        name: "expose",
        kind: FieldKind::Nested(&EXPOSE),
    },
    // `labels { "key": "value" }` (#243) completes the group: `router`,
    // `expose` and `traefik` decide which labels are *computed*, and
    // this one adds the ones no feature computes. It sits with them
    // rather than beside `env` — its nearest map-kind relative — because
    // what a reader needs to know about it is which other fields it can
    // collide with, and those are its three neighbors here.
    FieldSchema {
        name: "labels",
        kind: FieldKind::Nested(&LABELS),
    },
    FieldSchema {
        name: "restart",
        kind: FieldKind::Nested(&RESTART),
    },
    FieldSchema {
        name: "healthcheck",
        kind: FieldKind::Nested(&HEALTHCHECK),
    },
    FieldSchema {
        name: "publish",
        kind: FieldKind::Nested(&PUBLISH),
    },
    FieldSchema {
        name: "volume",
        kind: FieldKind::Nested(&VOLUME),
    },
    FieldSchema {
        name: "env",
        kind: FieldKind::Nested(&ENV),
    },
    FieldSchema {
        name: "raw",
        kind: FieldKind::Nested(&RAW),
    },
    // No `middleware` row: #221 moved it onto `router`, and #271 moved
    // routing itself out of the compiler, so a middleware is one more
    // label `std:traefik` writes. A body that still writes the field
    // here resolves through [`moved_field`] instead of falling through
    // to `UnknownField`.
    FieldSchema {
        name: "depends_on",
        kind: FieldKind::DependsOnList,
    },
    FieldSchema {
        name: "networks",
        kind: FieldKind::ReferenceList,
    },
    FieldSchema {
        name: "dns",
        kind: FieldKind::ReferenceList,
    },
    FieldSchema {
        name: "env_file",
        kind: FieldKind::ReferenceList,
    },
    // `privileged` (#157): a plain generic Compose key promoted out of
    // `raw` — see [`crate::ast::ServiceFields::privileged`] for the full
    // reasoning.
    FieldSchema {
        name: "privileged",
        kind: FieldKind::BoolFlag,
    },
    // `devices` (#157), map-kind since #167's review feedback — see
    // [`DEVICES`] and [`crate::ast::ServiceFields::devices`] for the
    // full reasoning.
    FieldSchema {
        name: "devices",
        kind: FieldKind::Nested(&DEVICES),
    },
    FieldSchema {
        name: "with",
        kind: FieldKind::Nested(&WITH),
    },
];

/// A top-level `service name { ... }` declaration.
pub static SERVICE: TypeSchema = TypeSchema {
    type_name: "service",
    kind: SchemaKind::Struct,
    fields: SERVICE_FIELDS,
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: true,
    schema_free: false,
};

/// A top-level `template name(params) { ... }` declaration. Shares
/// `SERVICE_FIELDS` with `SERVICE` (see that field's doc), but keeps its
/// own `type_name` so `UnknownField`/`DuplicateField` errors correctly
/// say "template" rather than "service".
pub static TEMPLATE: TypeSchema = TypeSchema {
    type_name: "template",
    kind: SchemaKind::Struct,
    fields: SERVICE_FIELDS,
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    needs_name: true,
    schema_free: false,
};

/// Looks up a top-level declaration's type schema by name. `template`
/// is handled separately by the parser, which matches its lexeme before
/// consulting this table (#258) — it is an ordinary `IDENT`, and
/// deliberately not a row here, so a declaration *named* `template`
/// still resolves against its own type's schema like any other.
///
/// `volume` appears both here (as [`VOLUME_DECL`], the top-level
/// declaration) and in `SERVICE_FIELDS` (as [`VOLUME`], the map-kind
/// mount field). That's not an ambiguity: this function is only ever
/// called on the first token of a *top-level* declaration, while a field
/// name is only ever resolved through [`resolve_field`] against the
/// enclosing type's own field list. Neither table is consulted in the
/// other's position, so the shared identifier resolves to exactly one
/// schema everywhere it can appear.
pub fn top_level_type(name: &str) -> Option<&'static TypeSchema> {
    match name {
        "network" => Some(&NETWORK),
        "volume" => Some(&VOLUME_DECL),
        "service" => Some(&SERVICE),
        _ => None,
    }
}

/// Whether `schema`'s body accepts a schema-free passthrough field (a
/// `Nested` field whose own type is [`TypeSchema::schema_free`] — today
/// exactly `raw`, on `service` and `template`).
///
/// Drives [`crate::ParseError::UnknownField`]'s `raw { ... }` hint, so
/// the hint is offered only in bodies where writing it would actually
/// compile. Derived from the schema rather than from a hardcoded list of
/// type names, so it can't drift away from the field tables above.
pub fn supports_raw(schema: &'static TypeSchema) -> bool {
    schema
        .fields
        .iter()
        .any(|f| matches!(f.kind, FieldKind::Nested(nested) if nested.schema_free))
}

/// Whether an `alias.name`-qualified [`crate::ast::Literal::Qualified`]
/// is *semantically* legal at `field_path` — the fully-dotted canonical
/// name every reference-shaped position uses elsewhere in this crate
/// (`"networks"`, `"router.entrypoints"`, `"router.path_prefix"`,
/// `"router.middleware"`, `"router.rule"` for a matcher argument,
/// `"dns"`, `"env_file"`, `"depends_on"`, or
/// `"volume"` for a named-volume mount's host side).
///
/// `false` for every position but the two listed here (#196). Before
/// #196's [`crate::ast::Literal`]/`Reference` unification this question
/// didn't need schema data at all: only `Reference`-typed positions
/// could ever *parse* a qualifier, and which of those resolved one
/// (`networks`, a named-volume host) versus rejected it (everything
/// else) was a hardcoded list of field names inline in
/// `compose::resolve_qualified_references`. Now that every
/// reference-shaped position parses the qualified form the same way —
/// see [`FieldKind::ReferenceList`]'s own doc — whether one is legal
/// once parsed has to live somewhere `compose::reject_qualified` can
/// consult by name instead, which is what this function is.
///
/// `networks` and `volume` are the only two `true` rows because they're
/// the only two with a real cross-file declaration to resolve a
/// qualifier against — a `network`/`volume` declaration another file
/// can `use ... as alias` and export. Every other row names something
/// with no `.hll`-declared existence to qualify at all (a Traefik entry
/// point or a router's middleware in the deployment's own
/// `traefik.yml`, a DNS
/// server address, an `env_file` path on disk, a same-file sibling
/// service for `depends_on`), so a qualified entry there is rejected
/// with [`crate::compose::ComposeError::UnsupportedQualifiedReference`]
/// rather than silently accepted with the qualifier dropped.
pub fn allows_qualified_reference(field_path: &str) -> bool {
    matches!(field_path, "networks" | "volume")
}

pub enum FieldResolution {
    Field(&'static FieldSchema),
    /// The type is schema-free (`raw`) and the key should be accepted as
    /// an arbitrary passthrough entry rather than looked up by name.
    RawPassthrough,
    /// The key names a field this type used to have and no longer does,
    /// carrying [`moved_field`]'s guidance on where it went. Distinct
    /// from [`Self::Unknown`] so the diagnostic can say so — see that
    /// function's own doc for why a bare "unknown field" is actively
    /// misleading here.
    Moved(&'static str),
    Unknown,
}

/// Guidance for a key that names a field `schema` used to have, or
/// `None` for a key that was never one of its fields.
///
/// A removed field is not the same mistake as a typo, and on a type
/// with a `raw` escape hatch saying so matters: [`crate::ParseError`]'s
/// `UnknownField` offers `raw { <key>: ... }` for anything it doesn't
/// recognize, which for a *moved* field is advice that compiles and
/// then emits a meaningless Compose key. `middleware` is exactly that
/// case — following the hint would write a `middleware:` key into the
/// service and silently drop the Traefik label the author wanted, which
/// for an authentication or IP-allowlist middleware is the "valid
/// output, wrong service" failure #144 already closed off elsewhere.
///
/// The two #199 rows are renames rather than moves, which is the same
/// mistake from the compiler's side: the name is gone, and an author
/// who reaches for it is better served by being told the spelling that
/// works than by "unknown field".
///
/// Every row here moved or was renamed pre-1.0, so write each guidance
/// clause for someone who never saw the old name — most readers who hit
/// one guessed the field belonged here rather than carrying it over
/// from an older file. Say where the field is, not when it left; the
/// rendering in [`crate::ParseError`] makes the same choice.
///
/// It's a function rather than a `FieldSchema` flag because a moved
/// field has no kind, no value grammar, and nothing for the parser to do
/// with it but refuse — it exists only as a name to recognize on the way
/// to a better error.
pub fn moved_field(schema: &'static TypeSchema, key_text: &str) -> Option<&'static str> {
    match (schema.type_name, key_text) {
        // #271 took routing out of the compiler. These are the names a
        // file written against any earlier release still carries, and
        // `UnknownField` would answer them with its `raw { ... }` hint —
        // actively wrong advice here, since `raw` replaces the whole
        // computed label list rather than adding to it. So the names
        // stay recognized purely to say where routing went.
        ("service" | "template", "router") => Some(
            "routing moved to `std:traefik`: `use \"std:traefik\" as traefik`, then \
             `with traefik.http { host: \"...\", port: ... }`",
        ),
        ("service" | "template", "traefik") => Some(
            "`traefik { disable }` moved to `std:traefik`: `use \"std:traefik\" as traefik`, \
             then `with traefik.disable`",
        ),
        ("service" | "template", "middleware") => Some(
            "a middleware is a label on one router now: `with traefik.http_middlewares \
             { router: \"{{name}}\", middlewares: [\"auth@file\"] }` from `std:traefik`",
        ),
        _ => None,
    }
}

/// The single function every field-name lookup in the parser goes
/// through — the generic engine's one lookup table, so a type's own
/// field list is the only thing that decides what a key resolves to.
pub fn resolve_field(schema: &'static TypeSchema, key_text: &str) -> FieldResolution {
    if let Some(field) = schema.fields.iter().find(|f| f.name == key_text) {
        return FieldResolution::Field(field);
    }
    if schema.schema_free {
        return FieldResolution::RawPassthrough;
    }
    if let Some(guidance) = moved_field(schema, key_text) {
        return FieldResolution::Moved(guidance);
    }
    FieldResolution::Unknown
}
