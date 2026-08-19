use std::fmt;

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
/// elsewhere as `alias.name` (see [`Reference::qualifier`] and
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

/// A literal value as written in source. The kind (string/number/bare
/// identifier) is preserved rather than normalized to a plain string,
/// since the grammar allows any of the three (`literal ::= STRING |
/// NUMBER | IDENT`) wherever a value is expected — e.g. `restart
/// unless-stopped` (bare `Ident`) and `restart "unless-stopped"` (`Str`)
/// are both legal and mean the same thing downstream, but keeping the
/// distinction costs nothing and supports future provenance/pretty-
/// printing needs.
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
    /// A `$name` parameter reference inside a `template`'s own body,
    /// naming one of that *same* template's own declared parameters,
    /// e.g. `$puid` in `template linuxserver_app(puid: Number, pgid:
    /// Number) { env PUID = $puid }`. Produced directly by the parser
    /// when it sees the `$` sigil — never by ordinary literal parsing,
    /// and never legal (a parse error) outside a template body, since a
    /// plain `service` isn't parameterized. Composition
    /// ([`fn@crate::compose`]) substitutes every `Param` with the
    /// invocation's bound argument value; a `Param` surviving composition
    /// would be a bug.
    Param(String, Span),
}

impl Literal {
    /// The literal's text content, regardless of which kind it is.
    pub fn text(&self) -> &str {
        match self {
            Literal::Str(s, _) | Literal::Ident(s, _) | Literal::Param(s, _) => s,
            Literal::Number { text, .. } => text,
        }
    }

    /// The literal's location in source.
    pub fn span(&self) -> Span {
        match self {
            Literal::Str(_, span) | Literal::Ident(_, span) | Literal::Param(_, span) => *span,
            Literal::Number { span, .. } => *span,
        }
    }
}

/// A declared template parameter's type. Deliberately small — `Number`
/// and `String` are the only two kinds a signature can declare this
/// milestone; bare-`Ident`-typed and list-typed parameters are out of
/// scope (see docs/DESIGN.md's Composition section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Number,
    String,
}

impl ParamType {
    /// The type's own name, as written in source (`Number`/`String`) and
    /// as shown in diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            ParamType::Number => "Number",
            ParamType::String => "String",
        }
    }
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// One declared template parameter: its name, plus an optional `:
/// Number|String` type annotation. An untyped parameter (`ty: None`)
/// accepts any literal kind at the call site with no compose-time check —
/// see [`crate::compose::ComposeError::ArgumentTypeMismatch`] for what a
/// typed parameter enforces.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Ident,
    pub ty: Option<ParamType>,
}

/// A bare-identifier reference, e.g. an entry in `middleware`/
/// `depends_on`/`networks`, or a `network` name referenced from a
/// `service` (not yet resolved/validated against declared networks this
/// milestone).
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// The `alias` in `alias.name`, from a `use`-imported file; `None`
    /// for a bare, same-file reference — the overwhelmingly common case,
    /// and the only kind that existed before imports.
    pub qualifier: Option<Ident>,
    pub name: String,
    /// The span of just the trailing `name` segment (after the `.`, if
    /// any). `span` below covers the whole reference including the
    /// qualifier.
    pub name_span: Span,
    pub span: Span,
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
/// a different, map-kind schema ([`VolumeMap`]) that happens to share
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

/// A parsed `expose` field. `port` is `Option` for the same reason as
/// [`Image::reference`] — see that doc.
#[derive(Debug, Clone, PartialEq)]
pub struct Expose {
    pub port: Option<Literal>,
    /// Settable either via the canonical `host: "..."` field or the
    /// `as "..."` bare-keyword alias sugar; both produce this same slot.
    pub host: Option<Literal>,
    /// The Traefik entry points to route through (e.g. `entrypoint
    /// web, web-secure`). A list rather than a scalar because Traefik's
    /// own `entrypoints=` label is a comma-separated list: making the
    /// language model that directly lets codegen own the joining, so
    /// no label value ever has to tolerate a user-written comma.
    ///
    /// Empty means the generated router gets no `entrypoints=` label at
    /// all — Traefik's own default of attaching to every entry point —
    /// rather than the parser guessing a homelab-specific value. That's
    /// why this is a plain `Vec` and not an `Option<Vec<_>>`: "unset"
    /// and "set to nothing" have to mean the same thing here, and
    /// `middleware` on [`ServiceFields`] already spells that shape.
    pub entrypoint: Vec<Reference>,
    pub span: Span,
}

/// A parsed `restart` field. `policy` is `Option` for the same reason as
/// [`Image::reference`] — see that doc.
#[derive(Debug, Clone, PartialEq)]
pub struct Restart {
    pub policy: Option<Literal>,
    pub span: Span,
}

/// `volume`'s entries. Uniqueness is checked on `container` (the value
/// side of `host -> container`), matching Docker's own constraint that
/// two mounts can't target the same container path even though the same
/// host path can be mounted at multiple container paths.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VolumeMap {
    pub entries: Vec<VolumeEntry>,
}

/// One `host -> container` mount entry.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeEntry {
    pub host: Literal,
    pub container: Literal,
    pub span: Span,
}

/// `publish`'s entries — host-port → container-port mappings, emitted as
/// Compose's `ports:` list. Uniqueness is checked on `container` (the
/// value side of `host -> container`), matching [`VolumeMap`]'s own
/// convention; see [`crate::schema::PUBLISH`] for why that side rather
/// than the host one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PublishMap {
    pub entries: Vec<PublishEntry>,
}

/// One `host -> container` published-port entry. Both sides are plain
/// [`Literal`]s rather than parsed port numbers: a quoted container side
/// is how a protocol suffix is written (`publish 53 -> "53/udp"`), and a
/// bare number is the ordinary case.
#[derive(Debug, Clone, PartialEq)]
pub struct PublishEntry {
    pub host: Literal,
    pub container: Literal,
    pub span: Span,
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
    pub expose: Option<Expose>,
    pub restart: Option<Restart>,
    /// `publish 8096 -> 8096` entries — Compose's `ports:` key. Distinct
    /// from `expose` above, which is Compose's `expose:` (visible to
    /// other containers on the same network, never published to the
    /// host) plus the Traefik router labels.
    pub publish: PublishMap,
    pub volumes: VolumeMap,
    pub env: EnvMap,
    pub raw: RawMap,
    pub middleware: Vec<Reference>,
    pub depends_on: Vec<Reference>,
    pub networks: Vec<Reference>,
    /// A per-service DNS resolver override (Compose's own `dns:` key,
    /// e.g. a LAN resolver IP) — a plain generic Compose key like
    /// `volume`/`env`/`expose`, not homelab-specific itself even though
    /// any one entry's value always is. List-typed and reference-list
    /// shaped like `middleware`/`depends_on`/`networks` (accumulates
    /// across repeats, never duplicate-checked), even though its entries
    /// are ordinary literal values (IP addresses) rather than references
    /// to another declaration — reusing [`Reference`] costs nothing here
    /// since a `STRING` entry (the only realistic way to write an IP,
    /// `IDENT`'s grammar can't contain a `.`) can never carry a
    /// qualifier anyway.
    pub dns: Vec<Reference>,
    /// Docker's own `container_name:` key. `None` means "default to the
    /// service's own name" (via the same `{{name}}` interpolation
    /// binding `expose`'s `as`-sugar already uses) — codegen, not the
    /// parser or composition, applies that default, mirroring how
    /// [`Network::real_name`] staying `Option` defers its own default to
    /// a later stage.
    pub container_name: Option<Literal>,
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
