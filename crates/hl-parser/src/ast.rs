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
/// `depends_on`/`networks`, the host side of a named-volume mount (see
/// [`VolumeHost::Named`]), or a `network` name referenced from a
/// `service`.
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
    pub reference: Reference,
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

/// `volume`'s entries. Uniqueness is checked on `container` (the value
/// side of `host -> container`), matching Docker's own constraint that
/// two mounts can't target the same container path even though the same
/// host path can be mounted at multiple container paths.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VolumeMap {
    pub entries: Vec<VolumeEntry>,
}

/// One `host -> container` mount entry, plus an optional trailing
/// `{ read_only }` flag (#158) that emits Compose short syntax's `:ro`
/// mode suffix (`/mnt/media:/data:ro`).
///
/// `read_only` is a plain `bool`, not an `Option<Span>` like
/// [`Network::external`]/[`Volume::external`]: those two report their own
/// span back through [`crate::ParseError::DuplicateField`] when the same
/// struct-kind body sets the flag twice, but a `volume` entry's body is
/// hand-parsed (see the parser's own `parse_mount_map_entry`)
/// rather than run through the generic struct-field engine, so there is
/// no second occurrence within one entry for a span to ever distinguish—
/// `{ read_only read_only }` is simply a syntax error at the second
/// token, never a value this type has to represent.
///
/// Syntax choice (#158): the issue's own two suggestions were a trailing
/// bare flag after the primary form (`volume "/" -> "/rootfs",
/// read_only`) or a `mode` sub-field body. The trailing-comma form was
/// rejected as genuinely ambiguous, not just unusual: inside `volume`'s
/// existing canonical multi-entry body (`volume { "/" -> "/rootfs",
/// "/var/run" -> "/var/run" }`), a comma already means "the next map
/// entry starts here," and a bare identifier is *already* legal there as
/// the host side of a named-volume entry (`volume { "/" -> "/rootfs",
/// read_only -> "/mnt2" }` legitimately mounts a volume literally named
/// `read_only`). Telling "a flag on the previous entry" from "the start
/// of a new entry that happens to run out of input before its own `->`"
/// apart would need unbounded lookahead the rest of the grammar never
/// asks for, for a distinction the parser can't make locally. A `{ ... }`
/// body sidesteps that entirely: the outer `{` that opens `volume`'s own
/// multi-entry body is only ever checked *before* any entry starts
/// parsing (the parser's own `parse_field`, `SchemaKind::Map` arm), so a
/// second, per-entry `{` after the container literal is never reachable
/// from that position and free of ambiguity. It also mirrors
/// existing precedent directly: `depends_on`'s own per-entry `{
/// condition: ... }` body (#155) is exactly this shape—`IDENT`
/// (optionally qualified) `->`/`:` value, then an optional `{ }` tail—so
/// `volume "/" -> "/rootfs" { read_only }` reuses a pattern this
/// language's readers already know rather than inventing a second one. A
/// `mode` sub-field (`{ mode: "ro" }`) was rejected on scope grounds
/// alone, not ambiguity: this milestone deliberately covers `:ro` only,
/// not Compose's other short-syntax suffixes (`:z`, `:Z`, tmpfs sizing),
/// and a bare presence flag says exactly that, with nothing left
/// unvalidated the way an arbitrary `mode` string would leave every
/// value but `"ro"` silently unchecked.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeEntry {
    pub host: VolumeHost,
    pub container: Literal,
    /// Whether this entry carried a `{ read_only }` body (#158). `false`
    /// for the overwhelming majority of entries, which carry no body at
    /// all—Compose's short syntax omits the `:ro` suffix entirely rather
    /// than spelling out `:rw`, and codegen matches that: this flag adds
    /// a suffix when set and changes nothing about the emitted string
    /// when it isn't (see `hl_codegen`'s `resolve_volumes`).
    pub read_only: bool,
    pub span: Span,
}

/// The host side of a `volume` entry: either a path on the machine
/// Compose runs on, or a reference to a named Docker volume declared by
/// a top-level `volume` declaration.
///
/// Which one is a *syntactic* question, decided here by the parser from
/// the token it read, not later by inspecting the string's shape: a
/// quoted string is always a path (`volume "/mnt/media" -> "/data"`,
/// `volume "./config" -> "/config"`), and a bare identifier is always a
/// reference (`volume syncthing-config -> "/config"`), exactly as a
/// `networks [traefik-net]` entry is. That's what lets a named volume
/// carry an `alias.name` qualifier at all — a string has no structure a
/// qualifier could attach to — and it makes the distinction one the
/// parser enforces rather than one codegen guesses.
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeHost {
    /// A quoted path (or any other non-identifier literal, including a
    /// `$param` a template substitutes a path into) bind-mounted from
    /// the host. Needs no declaration — Docker itself requires none for
    /// a host path.
    BindMount(Literal),
    /// A bare `IDENT`, optionally `alias.`-qualified, naming a top-level
    /// [`Volume`] declaration. Resolved exactly like a `networks [x]`
    /// entry: a bare name against the entry file's own declarations, a
    /// qualified one against the aliased module's.
    Named(Reference),
}

impl VolumeHost {
    /// The host's location in source.
    pub fn span(&self) -> Span {
        match self {
            VolumeHost::BindMount(lit) => lit.span(),
            VolumeHost::Named(r) => r.span,
        }
    }

    /// The host's text as written: the literal's content for a bind
    /// mount, the referenced declaration's name for a named volume
    /// (without any `alias.` qualifier, which names the file the
    /// declaration lives in rather than the volume).
    pub fn text(&self) -> &str {
        match self {
            VolumeHost::BindMount(lit) => lit.text(),
            VolumeHost::Named(r) => &r.name,
        }
    }
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
    /// Compose's own `healthcheck:` key (#153). See [`Healthcheck`]'s
    /// doc.
    pub healthcheck: Option<Healthcheck>,
    /// `publish 8096 -> 8096` entries — Compose's `ports:` key. Distinct
    /// from `expose` above, which is Compose's `expose:` (visible to
    /// other containers on the same network, never published to the
    /// host) plus the Traefik router labels.
    pub publish: PublishMap,
    pub volumes: VolumeMap,
    pub env: EnvMap,
    pub raw: RawMap,
    pub middleware: Vec<Reference>,
    /// `depends_on [db]` / `depends_on [db { condition: service_healthy }]`
    /// (#155) — each entry is a same-file service reference, optionally
    /// carrying an explicit Compose readiness condition. Unlike
    /// `middleware`/`networks`/`dns`/`env_file`, this isn't a plain
    /// [`Reference`] list: a `Reference` has nowhere to hang the
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
    /// `env_file "one.env"` / `env_file ["one.env", "two.env"]` — paths
    /// to load environment variables from, Compose's own `env_file:`
    /// key (#154). Same reasoning as [`Self::dns`] just above: a plain
    /// generic Compose key, not homelab-specific itself even though most
    /// real entries point at a gitignored, homelab-specific `.env` file,
    /// list-typed and reference-list shaped like
    /// `middleware`/`depends_on`/`networks`/`dns` (accumulates across
    /// repeats, never duplicate-checked, and a bare `env_file "one.env"`
    /// is sugar for a one-element list), even though its entries are
    /// ordinary path strings rather than references to another
    /// declaration. Reusing [`Reference`] costs nothing here for the
    /// same reason it costs nothing for `dns`: Compose's paths are
    /// resolved relative to the compose file, which is the user's
    /// concern, not `hllc`'s, so a path is carried through verbatim
    /// either way and never needs a qualifier to mean anything.
    pub env_file: Vec<Reference>,
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
    /// [`Reference`] lists, since Compose's `command:` key, like
    /// `healthcheck.test`, is either one bare string or a bracketed
    /// list, never a bare comma-separated sequence.
    pub command: Option<Command>,
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
