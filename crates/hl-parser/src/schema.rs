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
    /// A boolean flag settable only by bare presence (`external`, with no
    /// `: value` form this milestone) — its span is recorded when set.
    BoolFlag,
    /// The field's value is itself an instance of another registered
    /// type. Struct-kind nested types are single-occurrence (a second
    /// write is `DuplicateField`); map-kind nested types accumulate
    /// entries across repeated writes (per docs/DESIGN.md's rule 4).
    Nested(&'static TypeSchema),
    /// A list of bare-identifier/string references (`middleware`,
    /// `depends_on`, `networks`). Accumulates across repeats; settable
    /// via a bracketed list, the bare comma-list sugar, or repeated
    /// statements — never duplicate-checked, since list fields can't
    /// collide.
    ReferenceList,
    /// A list of template invocations (`with`'s `templates` field): each
    /// item is an `IDENT` naming a template, optionally followed by a
    /// `{ arg: value, ... }` argument body. Parses like [`ReferenceList`]
    /// (bracketed list, bare comma-list sugar, accumulates, never
    /// duplicate-checked) except each item can carry an argument body.
    TemplateInvocationList,
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `expose 8096 as "host"` / `expose { port: 8096, host: "host", entrypoint: "web-secure" }`.
/// `entrypoint` names a Traefik entry point (e.g. `"web-secure"`) — left
/// unset, codegen omits the `entrypoints=` label entirely rather than
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
            kind: FieldKind::Scalar,
        },
    ],
    primary_field: Some("port"),
    map_separator: None,
    uniqueness: None,
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
    bare_keyword_alias: None,
    needs_name: false,
    schema_free: false,
};

/// `volume "host" -> "container"` / `volume { "host": "container" }`.
/// Uniqueness on the value (container path) side, matching Docker's own
/// same-container-path constraint.
pub static VOLUME: TypeSchema = TypeSchema {
    type_name: "volume",
    kind: SchemaKind::Map,
    fields: &[],
    primary_field: None,
    map_separator: Some(TokenKind::Arrow),
    uniqueness: Some(MapSide::Value),
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
    FieldSchema {
        name: "expose",
        kind: FieldKind::Nested(&EXPOSE),
    },
    FieldSchema {
        name: "restart",
        kind: FieldKind::Nested(&RESTART),
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
        kind: FieldKind::ReferenceList,
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
    bare_keyword_alias: None,
    needs_name: true,
    schema_free: false,
};

/// Looks up a top-level declaration's type schema by name. `template` is
/// handled separately by the parser as a lexical-token check (`template`
/// is a reserved word, not an ordinary `IDENT`), not via this table.
pub fn top_level_type(name: &str) -> Option<&'static TypeSchema> {
    match name {
        "network" => Some(&NETWORK),
        "service" => Some(&SERVICE),
        _ => None,
    }
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
