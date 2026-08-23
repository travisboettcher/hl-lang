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
    /// A list of bare-identifier/string references (`middleware`,
    /// `networks`, `dns`, `env_file`). Accumulates across
    /// repeats; settable via a bracketed list, the bare comma-list
    /// sugar, or repeated statements — never duplicate-checked, since
    /// list fields can't collide.
    ///
    /// `devices` moved off this kind and onto [`SchemaKind::Map`] (see
    /// [`DEVICES`]) once #167's review feedback asked for the same
    /// `"host" -> "container"` arrow spelling `publish`/`volume` already
    /// use, rather than a pre-joined `"host:container"` string.
    ///
    /// `depends_on` moved off this kind and onto its own
    /// [`Self::DependsOnList`] when its entries gained an optional
    /// `{ condition: ... }` body (#155) — a plain [`crate::ast::Reference`]
    /// has nowhere to hang that.
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
    /// Which side of a map entry is uniqueness-checked; `None` only for
    /// `raw`, which is schema-free and checks nothing.
    pub uniqueness: Option<MapSide>,
    /// Map-kind types only: whether a bare `IDENT` on the *key* side of
    /// an entry is a reference to a top-level declaration
    /// ([`crate::ast::VolumeHost::Named`]) rather than an ordinary
    /// literal — which also lets it carry an `alias.` qualifier.
    ///
    /// True for [`VOLUME`] alone. `env`/`publish`/`driver_opts`/`raw`
    /// keys are plain literal values with nothing to resolve against, so
    /// they keep the ordinary all-literal entry parsing.
    pub key_may_be_reference: bool,
    /// A bare keyword (no colon) that aliases to a named secondary
    /// field, e.g. `("as", "host")` on `expose`: `expose 8096 as "..."`
    /// desugars to `expose { port: 8096, host: "..." }`. Kept as schema
    /// data (not a hardcoded per-type branch in the parser) so the
    /// engine stays generic.
    pub bare_keyword_alias: Option<(&'static str, &'static str)>,
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `expose 8096 as "host"` / `expose { port: 8096, host: "host", entrypoint: web-secure }`.
/// `entrypoint` is a reference *list* of Traefik entry-point names
/// (`entrypoint web, web-secure`), spelled exactly like `middleware`,
/// because Traefik's `entrypoints=` label is itself comma-separated —
/// modelling that as a list keeps the comma codegen's to write rather
/// than the user's, so no label value has to permit one. Left empty,
/// codegen omits the `entrypoints=` label entirely rather than
/// defaulting to any specific value, matching Traefik's own real
/// behavior ("no entry points" attaches to all of them) since any one
/// entry-point name is specific to a given homelab's own `traefik.yml`,
/// not a generic default the compiler should assume.
pub static EXPOSE: TypeSchema = TypeSchema {
    type_name: "expose",
    kind: SchemaKind::Struct,
    fields: &[
        FieldSchema {
            name: "port",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "host",
            kind: FieldKind::Scalar,
        },
        FieldSchema {
            name: "entrypoint",
            kind: FieldKind::ReferenceList,
        },
    ],
    primary_field: Some("port"),
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    bare_keyword_alias: Some(("as", "host")),
    needs_name: false,
    schema_free: false,
};

/// `restart unless-stopped` / `restart { policy: "unless-stopped" }`.
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
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `traefik { disabled }` — the one way to opt a service out of every
/// Traefik label `hl-codegen`'s `labels.rs` otherwise computes for it
/// (#159). `disabled` mirrors [`HEALTHCHECK`]'s `disable` and
/// [`NETWORK`]'s `external` exactly: a bare-presence
/// [`FieldKind::BoolFlag`], with no `disabled: false` form this
/// milestone.
///
/// **Rejected alternative: `traefik disabled`, no braces.** The issue
/// that motivated this field (#159) floats that spelling first, but it
/// doesn't fit the schema engine without bending it. A bare, brace-free
/// form only exists for a type with a `primary_field`
/// (docs/DESIGN.md's desugaring rule 1), and
/// `parse_struct_primary_shorthand`'s only bare-value path parses a
/// *literal* (`self.parse_literal()`) — it has no notion of "the bare
/// word names one of my own sub-fields," which is what `disabled` would
/// have to mean here. Making `disabled` a primary field can't work
/// either way that keeps faith with what a primary field means
/// elsewhere: `FieldKind::BoolFlag` carries no value beyond its own bare
/// presence, so there's no *value* for `traefik disabled` to hand the
/// primary-shorthand parser, only a second field name masquerading as
/// one. Reaching `traefik disabled` regardless would mean either
/// treating the identifier `disabled` as a magic scalar payload (special
/// syntax for this one field, invisible to `resolve_field`) or teaching
/// the primary-shorthand parser a "bare keyword names a sub-field"
/// grammar no other type uses — both routes bend the generic engine
/// around one field instead of reusing it, which is exactly what
/// `schema.rs`'s table-driven design exists to avoid (see this module's
/// own doc). `traefik { disabled }` costs nothing beyond what
/// `healthcheck { disable }` and `network n { external }` already pay
/// for, and — being `Nested` rather than a bare `BoolFlag` field
/// directly on `SERVICE_FIELDS` — leaves a namespace open for a future
/// Traefik knob (a router priority, a TLS resolver name, ...) to land in
/// without inventing a second `traefik`-prefixed field name or promoting
/// this to its own top-level type.
pub static TRAEFIK: TypeSchema = TypeSchema {
    type_name: "traefik",
    kind: SchemaKind::Struct,
    fields: &[FieldSchema {
        name: "disabled",
        kind: FieldKind::BoolFlag,
    }],
    primary_field: None,
    map_separator: None,
    uniqueness: None,
    key_may_be_reference: false,
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// The `volume` *field* on a `service`/`template`: `volume "/host/path"
/// -> "/container"` / `volume { "/host/path": "/container" }` for a bind
/// mount, `volume named-volume -> "/container"` for a named one.
/// Uniqueness on the value (container path) side, matching Docker's own
/// same-container-path constraint.
///
/// The one map-kind type with [`TypeSchema::key_may_be_reference`] set:
/// a bare `IDENT` on the host side is a reference to a top-level
/// `volume` declaration rather than a literal, so it can be
/// `alias.`-qualified like any other cross-file reference. See
/// [`crate::ast::VolumeHost`].
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `publish 8096 -> 8096` / `publish { 8096: 8096 }` — a host-port →
/// container-port mapping, emitted as Compose's `ports:` list. Spelled
/// with the same `->` separator as [`VOLUME`] because it's the same
/// shape: a host-side resource mapped onto a container-side one. Kept
/// entirely separate from [`EXPOSE`], which keeps its own meaning
/// (Compose's `expose:` — container-network visibility only, plus the
/// Traefik router labels) unchanged.
///
/// Uniqueness on the value (container port) side, matching [`VOLUME`]'s
/// own `host -> container` convention rather than the host side. Docker's
/// real conflict is on the host side, but a protocol suffix rides on the
/// container half of a Compose short-syntax mapping (`53:53/udp`), so a
/// host-side check would reject the very configuration this field exists
/// to make expressible — Pi-hole publishing both `53 -> "53/tcp"` and
/// `53 -> "53/udp"`. Checking the container side still catches the
/// copy-paste case (the same target port written twice) and leaves the
/// legitimate one alone.
pub static PUBLISH: TypeSchema = TypeSchema {
    type_name: "publish",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
    key_may_be_reference: false,
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `devices "/dev/kmsg" -> "/dev/kmsg"` / `devices { "/dev/kmsg":
/// "/dev/kmsg" }` — a host device path → container device path mapping,
/// emitted as Compose's `devices:` list. Spelled with the same `->`
/// separator as [`VOLUME`]/[`PUBLISH`] because it's the same shape once
/// more: a host-side resource mapped onto a container-side one. Grew
/// this arrow spelling in place of a pre-joined `"host:container"`
/// string per review feedback on #167, after #157 originally shipped it
/// as a [`FieldKind::ReferenceList`].
///
/// Uniqueness on the value (container path) side — exactly [`PUBLISH`]'s
/// own reasoning, and it transfers over unchanged. Docker's real
/// conflict on a `devices` short-syntax entry is on the host side, but
/// Compose's short syntax is `HOST:CONTAINER[:CGROUP_PERMISSIONS]`, so
/// an optional `rwm`-style permissions suffix rides the *container*
/// half (`"/dev/sda" -> "/dev/xvda:rwm"`) — the direct analogue of
/// `publish`'s protocol suffix riding its own container half (`53 ->
/// "53/udp"`). A host-side uniqueness check would reject the legitimate
/// case this makes expressible: the same host device mapped to two
/// different container paths, each with its own permissions. Checking
/// the container side still catches the copy-paste case (the same
/// target path written twice) and leaves the legitimate one alone.
pub static DEVICES: TypeSchema = TypeSchema {
    type_name: "devices",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
    key_may_be_reference: false,
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `raw { any_key: any_value }` — schema-free passthrough, no unknown-key
/// or uniqueness checking.
pub static RAW: TypeSchema = TypeSchema {
    type_name: "raw",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Colon),
    uniqueness: None,
    key_may_be_reference: false,
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
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
    // The name is shared with [`EXPOSE`]'s own `entrypoint` sub-field,
    // which is an unrelated reference list of Traefik entry-point
    // names. That's the same two-roles-one-identifier situation
    // `volume` is already in (see [`top_level_type`]'s doc), and it
    // stays unambiguous for the same reason: a field name is only ever
    // resolved through [`resolve_field`] against the enclosing type's
    // own field list, so `entrypoint` written in a `service`/`template`
    // body resolves here and `entrypoint` written inside an `expose`
    // body or after an `expose` shorthand's comma resolves against
    // [`EXPOSE`]. Neither table is consulted in the other's position.
    FieldSchema {
        name: "entrypoint",
        kind: FieldKind::ScalarOrList,
    },
    FieldSchema {
        name: "expose",
        kind: FieldKind::Nested(&EXPOSE),
    },
    // `traefik { disabled }` (#159) sits next to `expose` on purpose:
    // the two jointly decide whether — and how — a service gets a
    // Traefik router, so `hl-codegen`'s `labels.rs` reads them as one
    // related pair.
    FieldSchema {
        name: "traefik",
        kind: FieldKind::Nested(&TRAEFIK),
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
    FieldSchema {
        name: "middleware",
        kind: FieldKind::ReferenceList,
    },
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
    bare_keyword_alias: None,
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
    bare_keyword_alias: None,
    needs_name: true,
    schema_free: false,
};

/// Looks up a top-level declaration's type schema by name. `template` is
/// handled separately by the parser as a lexical-token check (`template`
/// is a reserved word, not an ordinary `IDENT`), not via this table.
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

pub enum FieldResolution {
    Field(&'static FieldSchema),
    /// The type is schema-free (`raw`) and the key should be accepted as
    /// an arbitrary passthrough entry rather than looked up by name.
    RawPassthrough,
    Unknown,
}

/// The single function every field-name lookup in the parser goes
/// through. This is what keeps the `as`→`host` alias driven by schema
/// data instead of hardcoded per-type branches in the parser body.
pub fn resolve_field(schema: &'static TypeSchema, key_text: &str) -> FieldResolution {
    if let Some(field) = schema.fields.iter().find(|f| f.name == key_text) {
        return FieldResolution::Field(field);
    }
    if let Some((keyword, target)) = schema.bare_keyword_alias
        && key_text == keyword
    {
        let field = schema
            .fields
            .iter()
            .find(|f| f.name == target)
            .expect("bare_keyword_alias target must exist in the type's own field list");
        return FieldResolution::Field(field);
    }
    if schema.schema_free {
        return FieldResolution::RawPassthrough;
    }
    FieldResolution::Unknown
}
