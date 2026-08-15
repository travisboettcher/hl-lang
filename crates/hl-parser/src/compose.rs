//! Resolves `template`/`with` composition — the pass that runs on
//! [`crate::parse`]'s output to turn a service's own body plus whatever
//! templates it pulls in via `with` into one fully-merged
//! [`crate::Service`], per docs/DESIGN.md's "Composition" section.
//!
//! Kept as a separate pass from `parse()` deliberately: `parse()` stays
//! purely syntactic (known fields, correct kinds, no illegal
//! duplicates), while template resolution — a semantic concern, same as
//! "required fields" already being deferred past `parse()` — lives here.
//!
//! The merge engine itself ([`compose_with_resolver`] and everything
//! below it) is generalized over a [`SymbolResolver`], so it can resolve
//! both same-file names (the plain [`compose`] entry point, via
//! [`SingleFileResolver`]) and cross-file `alias.name` references (a
//! future linker, over its own module graph) with one implementation —
//! see [`SymbolResolver`]'s own doc for the scoping contract that makes
//! a template's own references always resolve in the scope it was
//! *declared* in, not the scope it's invoked from.

use std::collections::HashMap;
use std::fmt;

use hl_lexer::Span;

use crate::ast::{
    EnvEntry, EnvMap, Expose, Ident, Image, Literal, Network, Program, RawMap, RawValue, Reference,
    Restart, Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, VolumeEntry,
    VolumeMap,
};
use crate::schema::MapSide;

/// The result of resolving every `template`/`with` composition in a
/// [`Program`] — see [`compose`]. Every `Service` here has an empty
/// `fields.with` and contains no [`Literal::Param`] anywhere; producing
/// that is this module's whole job.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposedProgram {
    pub networks: Vec<Network>,
    pub services: Vec<Service>,
}

/// An error raised while resolving `template`/`with` composition.
/// Mirrors [`crate::ParseError`]'s design (structured, span-carrying, no
/// error recovery — resolution stops at the first error).
#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    /// `with X` (or the implicit `defaults`) names a template with no
    /// matching top-level `template` declaration anywhere in the program.
    UnknownTemplate { name: String, span: Span },
    /// Two top-level `template` declarations share a name — unlike
    /// service/network names, template names are actually looked up, so
    /// they must be unique.
    DuplicateTemplateName {
        name: String,
        first: Span,
        second: Span,
    },
    /// A `with`-reference chain returns to a template already being
    /// resolved (e.g. `template a { with b }` / `template b { with a }`).
    /// `chain` lists the template names in resolution order, ending with
    /// the name that closes the cycle.
    TemplateCycle { chain: Vec<String>, span: Span },
    /// An invocation's argument name isn't one of the target template's
    /// declared parameters. Per docs/DESIGN.md: "Templates must be fully
    /// applied at each call — no partial application, no currying."
    UnknownTemplateArgument {
        template: String,
        argument: String,
        span: Span,
    },
    /// A template parameter has no corresponding argument in the
    /// invocation — the other direction of the same "fully applied" rule.
    MissingTemplateArgument {
        template: String,
        param: String,
        span: Span,
    },
    /// The same argument name appears twice in one invocation's `{ }`
    /// body. Checked explicitly here because invocation arguments reuse
    /// `raw`'s schema-free [`RawMap`], which has no uniqueness checking
    /// of its own.
    DuplicateTemplateArgument {
        template: String,
        argument: String,
        first: Span,
        second: Span,
    },
    /// A parameter used in a plain scalar `Literal` slot (e.g.
    /// `expose.port`) was invoked with a list/nested-map argument. No
    /// evaluation or coercion is performed — hl-lang is a transpiler, not
    /// an interpreter — so this is a hard error rather than a silent
    /// flatten/stringify.
    TemplateArgumentNotScalar {
        template: String,
        param: String,
        span: Span,
    },
    /// Two *explicit* `with`-listed templates both set the same
    /// scalar/struct field (`image`/`expose`/`restart`). Per
    /// docs/DESIGN.md: "a collision between two of these on the same
    /// scalar/map field is a compile error." Never raised against
    /// `defaults` (always silently loses) or the service's own body
    /// (always silently wins).
    FieldCollision {
        field: &'static str,
        first_template: String,
        second_template: String,
        first: Span,
        second: Span,
    },
    /// Same rule as [`Self::FieldCollision`], for a map field
    /// (`env`/`volume`) — two explicit templates set the same key
    /// (`env`) or container path (`volume`, matching that field's
    /// existing [`MapSide::Value`] uniqueness convention). Boxed since
    /// this is by far `ComposeError`'s largest variant (five owned
    /// fields) and every other variant is much smaller.
    MapKeyCollision(Box<MapKeyCollision>),
    /// An `alias.name` reference's `alias` doesn't resolve to anything —
    /// either no `use ... as alias` was ever in scope, or (this
    /// milestone specifically) the reference was resolved by
    /// [`SingleFileResolver`], which has no aliases at all: a lone
    /// [`Program`] has no imports by definition.
    UnknownAlias { alias: String, span: Span },
    /// A qualified reference (`alias.name`) was used on `middleware` or
    /// `depends_on` — neither has a coherent cross-file meaning yet
    /// (`depends_on` names a same-file sibling service; `middleware`
    /// isn't resolved against anything at all today), so this is
    /// rejected rather than silently accepted or silently dropped.
    UnsupportedQualifiedReference {
        field: &'static str,
        alias: String,
        span: Span,
    },
    /// A qualified `networks [alias.name]` entry's `alias` resolved to a
    /// real imported scope, but no `network` named `name` exists there.
    /// Distinct from [`Self::UnknownAlias`] (the alias itself didn't
    /// resolve) — [`SingleFileResolver`] never raises this, since every
    /// qualified lookup there is unconditionally `UnknownAlias` (a lone
    /// [`Program`] has no valid aliases at all); a real cross-file
    /// resolver is the first place this becomes reachable.
    UnknownQualifiedNetwork {
        alias: String,
        name: String,
        span: Span,
    },
}

/// Details for [`ComposeError::MapKeyCollision`], boxed out of the enum
/// to keep `ComposeError` itself small.
#[derive(Debug, Clone, PartialEq)]
pub struct MapKeyCollision {
    pub field: &'static str,
    pub side: MapSide,
    pub key: String,
    pub first_template: String,
    pub second_template: String,
    pub first: Span,
    pub second: Span,
}

impl ComposeError {
    /// Where the error occurred. For "first set here" style errors this
    /// is the *second* (offending) occurrence, mirroring
    /// [`crate::ParseError::span`].
    pub fn span(&self) -> Span {
        match self {
            ComposeError::UnknownTemplate { span, .. }
            | ComposeError::DuplicateTemplateName { second: span, .. }
            | ComposeError::TemplateCycle { span, .. }
            | ComposeError::UnknownTemplateArgument { span, .. }
            | ComposeError::MissingTemplateArgument { span, .. }
            | ComposeError::DuplicateTemplateArgument { second: span, .. }
            | ComposeError::TemplateArgumentNotScalar { span, .. }
            | ComposeError::FieldCollision { second: span, .. }
            | ComposeError::UnknownAlias { span, .. }
            | ComposeError::UnsupportedQualifiedReference { span, .. }
            | ComposeError::UnknownQualifiedNetwork { span, .. } => *span,
            ComposeError::MapKeyCollision(details) => details.second,
        }
    }
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span = self.span();
        match self {
            ComposeError::UnknownTemplate { name, .. } => {
                write!(f, "{}:{}: unknown template `{name}`", span.line, span.col)
            }
            ComposeError::DuplicateTemplateName { name, first, .. } => write!(
                f,
                "{}:{}: duplicate template `{name}` (first declared at {}:{})",
                span.line, span.col, first.line, first.col
            ),
            ComposeError::TemplateCycle { chain, .. } => write!(
                f,
                "{}:{}: template composition cycle: {}",
                span.line,
                span.col,
                chain.join(" -> ")
            ),
            ComposeError::UnknownTemplateArgument {
                template, argument, ..
            } => write!(
                f,
                "{}:{}: unknown argument `{argument}` for template `{template}`",
                span.line, span.col
            ),
            ComposeError::MissingTemplateArgument {
                template, param, ..
            } => write!(
                f,
                "{}:{}: missing required argument `{param}` for template `{template}`",
                span.line, span.col
            ),
            ComposeError::DuplicateTemplateArgument {
                template,
                argument,
                first,
                ..
            } => write!(
                f,
                "{}:{}: duplicate argument `{argument}` for template `{template}` (first set at {}:{})",
                span.line, span.col, first.line, first.col
            ),
            ComposeError::TemplateArgumentNotScalar {
                template, param, ..
            } => write!(
                f,
                "{}:{}: argument `{param}` for template `{template}` must be a scalar value (a list/map can't fill a single-value field)",
                span.line, span.col
            ),
            ComposeError::FieldCollision {
                field,
                first_template,
                second_template,
                first,
                ..
            } => write!(
                f,
                "{}:{}: field `{field}` set by both template `{first_template}` (at {}:{}) and template `{second_template}` — explicit templates must not conflict",
                span.line, span.col, first.line, first.col
            ),
            ComposeError::MapKeyCollision(details) => {
                let side_desc = match details.side {
                    MapSide::Key => "key",
                    MapSide::Value => "value",
                };
                write!(
                    f,
                    "{}:{}: `{}` {side_desc} {:?} set by both template `{}` (at {}:{}) and template `{}` — explicit templates must not conflict",
                    span.line,
                    span.col,
                    details.field,
                    details.key,
                    details.first_template,
                    details.first.line,
                    details.first.col,
                    details.second_template,
                )
            }
            ComposeError::UnknownAlias { alias, .. } => {
                write!(f, "{}:{}: unknown alias `{alias}`", span.line, span.col)
            }
            ComposeError::UnsupportedQualifiedReference { field, alias, .. } => write!(
                f,
                "{}:{}: `{field}` doesn't support a qualified reference yet (`{alias}.` ...)",
                span.line, span.col
            ),
            ComposeError::UnknownQualifiedNetwork { alias, name, .. } => write!(
                f,
                "{}:{}: no network `{name}` in `{alias}`",
                span.line, span.col
            ),
        }
    }
}

impl std::error::Error for ComposeError {}

/// Resolves names against a whole-program symbol table, generalized over
/// an opaque `Scope` so the same merge engine ([`compose_with_resolver`])
/// works both for a single already-parsed [`Program`] (via
/// [`SingleFileResolver`], `Scope = ()`) and, in a future milestone, a
/// whole module graph of cross-file `use` imports (`Scope` = a module
/// identity).
///
/// **Scoping contract** (this is what makes docs/DESIGN.md's import
/// scoping rule work: a template's own references resolve relative to
/// the file/scope it's *declared* in, never the scope of whoever
/// eventually invokes it): [`Self::resolve_template`] returns the
/// target's *own* declaring scope alongside the declaration. Callers
/// must resolve that declaration's own body using the *returned* scope,
/// never the scope the lookup was performed from.
pub trait SymbolResolver {
    type Scope: Copy + Eq + std::hash::Hash;

    /// Resolves an explicit `with`-list invocation's target template.
    /// `qualifier` is `Some` for `with alias.name`, `None` for a bare
    /// `with name`. Always an error if nothing matches — unlike
    /// [`Self::resolve_defaults`], every call here corresponds to an
    /// explicit, user-written invocation.
    fn resolve_template(
        &self,
        scope: Self::Scope,
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
    ) -> Result<(Self::Scope, &TemplateDecl), ComposeError>;

    /// Looks up the implicit `defaults` template in `scope`. `None`, not
    /// an error, if none is declared — unlike [`Self::resolve_template`],
    /// this never corresponds to an explicit invocation the user wrote.
    fn resolve_defaults(&self, scope: Self::Scope) -> Option<(Self::Scope, &TemplateDecl)>;

    /// Resolves a *qualified* `networks [alias.name]` entry. Never called
    /// for a bare/unqualified entry — those are left completely
    /// untouched, exactly as before imports existed.
    fn resolve_qualified_network(
        &self,
        scope: Self::Scope,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Network, ComposeError>;
}

/// The [`SymbolResolver`] backing the plain [`compose`] entry point: a
/// single already-parsed [`Program`]'s own template symbol table, no
/// imports at all. Its `Scope` is `()` since there's only ever one scope
/// to resolve within. Any *qualified* reference is answered with
/// [`ComposeError::UnknownAlias`] — correct and honest, not a
/// placeholder: a lone `Program` has no imports by definition, so no
/// alias can ever be valid here.
struct SingleFileResolver {
    templates: HashMap<String, TemplateDecl>,
}

impl SymbolResolver for SingleFileResolver {
    type Scope = ();

    fn resolve_template(
        &self,
        _scope: (),
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
    ) -> Result<((), &TemplateDecl), ComposeError> {
        if let Some(q) = qualifier {
            return Err(ComposeError::UnknownAlias {
                alias: q.name.clone(),
                span: q.span,
            });
        }
        self.templates
            .get(name)
            .map(|decl| ((), decl))
            .ok_or_else(|| ComposeError::UnknownTemplate {
                name: name.to_string(),
                span,
            })
    }

    fn resolve_defaults(&self, _scope: ()) -> Option<((), &TemplateDecl)> {
        self.templates.get("defaults").map(|decl| ((), decl))
    }

    fn resolve_qualified_network(
        &self,
        _scope: (),
        qualifier: &Ident,
        _name: &str,
        _span: Span,
    ) -> Result<&Network, ComposeError> {
        Err(ComposeError::UnknownAlias {
            alias: qualifier.name.clone(),
            span: qualifier.span,
        })
    }
}

/// Resolves every `template`/`with` composition in `program`, using only
/// `program`'s own top-level declarations — no cross-file imports are
/// followed (see [`SingleFileResolver`]'s doc: a `use` declaration
/// parses, but any *qualified* reference it enables errors with
/// [`ComposeError::UnknownAlias`], since a lone `Program` has nowhere to
/// resolve it). Templates are collected into a whole-program symbol
/// table first (so `with` can reference a template declared anywhere in
/// the file, not just earlier in it), then each service's `with`-list is
/// merged per docs/DESIGN.md's 3-tier priority: the implicit `defaults`
/// template (if declared) at the lowest tier, each explicit
/// `with`-listed template left-to-right above that (collisions between
/// two of these are errors), and the service's own body on top (always
/// wins, unconditionally). Resolution stops at the first error, matching
/// [`crate::parse`]'s own no-error-recovery precedent.
pub fn compose(program: Program) -> Result<ComposedProgram, ComposeError> {
    let mut networks = Vec::new();
    let mut services = Vec::new();
    let mut templates: HashMap<String, TemplateDecl> = HashMap::new();

    for decl in program.decls {
        match decl {
            TopDecl::Network(n) => networks.push(n),
            TopDecl::Service(s) => services.push(*s),
            TopDecl::Template(t) => {
                if let Some(prev) = templates.get(&t.name.name) {
                    return Err(ComposeError::DuplicateTemplateName {
                        name: t.name.name.clone(),
                        first: prev.name.span,
                        second: t.name.span,
                    });
                }
                templates.insert(t.name.name.clone(), *t);
            }
            // A `use` declaration is inert here — same as an unused
            // `network`/`template` decl, it's only meaningful if
            // something actually references it, and a lone `Program`
            // has no way to follow it regardless (see
            // `SingleFileResolver`'s doc).
            TopDecl::Use(_) => {}
        }
    }

    let resolver = SingleFileResolver { templates };
    compose_with_resolver(networks, services, (), &resolver)
}

/// The generalized merge engine: composes `services` (plus whatever
/// `networks` their qualified `networks [...]` entries additionally
/// resolve to) by resolving every name through `resolver`, starting from
/// `entry_scope`. See [`SymbolResolver`]'s doc for the scoping contract
/// this implements.
pub fn compose_with_resolver<R: SymbolResolver>(
    networks: Vec<Network>,
    services: Vec<Service>,
    entry_scope: R::Scope,
    resolver: &R,
) -> Result<ComposedProgram, ComposeError> {
    let mut cache: HashMap<(R::Scope, String), ServiceFields> = HashMap::new();
    let mut extra_networks: Vec<Network> = Vec::new();
    let mut composed = Vec::with_capacity(services.len());
    for service in services {
        composed.push(compose_service(
            service,
            entry_scope,
            resolver,
            &mut cache,
            &mut extra_networks,
        )?);
    }

    let mut all_networks = networks;
    all_networks.extend(extra_networks);
    Ok(ComposedProgram {
        networks: all_networks,
        services: composed,
    })
}

fn compose_service<R: SymbolResolver>(
    service: Service,
    scope: R::Scope,
    resolver: &R,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    extra_networks: &mut Vec<Network>,
) -> Result<Service, ComposeError> {
    let mut acc = MergeAcc::default();
    let mut in_progress = Vec::new();

    if let Some((defaults_scope, defaults_decl)) = resolver.resolve_defaults(scope) {
        let resolved = resolve_template(
            defaults_decl,
            defaults_scope,
            resolver,
            cache,
            &mut in_progress,
            extra_networks,
        )?;
        merge_tier(&mut acc, resolved, &Tier::Defaults)?;
    }

    for inv in &service.fields.with {
        let resolved = resolve_invocation(
            inv,
            scope,
            resolver,
            cache,
            &mut in_progress,
            extra_networks,
        )?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }

    let mut own = service.fields;
    own.with.clear();
    resolve_qualified_networks(&mut own, scope, resolver, extra_networks)?;
    merge_tier(&mut acc, own, &Tier::Own)?;

    Ok(Service {
        name: service.name,
        fields: acc.into_service_fields(),
        span: service.span,
    })
}

/// Resolves a *template's own* composition: its own `with`-list merged
/// (explicit-tier, left-to-right) with its own directly-set fields
/// (always winning over its own `with`-list — the same rule as a
/// service's own body, minus `defaults`, which is confirmed to apply
/// only at the final service-level merge, never inside template-internal
/// resolution). The result is cached by `(scope, name)` — still in
/// *parameterized* form (any `Literal::Param` the template's own body
/// declared is left untouched) — since the same template can be invoked
/// multiple times with different concrete arguments; substitution always
/// happens on a fresh clone in [`resolve_invocation`], never mutating the
/// cache. Keying by `scope` as well as `name` (not just `name`) is
/// required, not optional: two different scopes each declaring a
/// same-named template must resolve completely independently.
fn resolve_template<'r, R: SymbolResolver>(
    decl: &'r TemplateDecl,
    scope: R::Scope,
    resolver: &'r R,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    in_progress: &mut Vec<(R::Scope, String)>,
    extra_networks: &mut Vec<Network>,
) -> Result<ServiceFields, ComposeError> {
    let name = &decl.name.name;
    let cache_key = (scope, name.clone());
    if let Some(fields) = cache.get(&cache_key) {
        return Ok(fields.clone());
    }
    if in_progress.iter().any(|(s, n)| *s == scope && n == name) {
        let mut chain: Vec<String> = in_progress.iter().map(|(_, n)| n.clone()).collect();
        chain.push(name.clone());
        return Err(ComposeError::TemplateCycle {
            chain,
            span: decl.name.span,
        });
    }
    in_progress.push((scope, name.clone()));

    let mut acc = MergeAcc::default();
    for inv in &decl.fields.with {
        let resolved =
            resolve_invocation(inv, scope, resolver, cache, in_progress, extra_networks)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }
    let mut own = decl.fields.clone();
    own.with.clear();
    resolve_qualified_networks(&mut own, scope, resolver, extra_networks)?;
    merge_tier(&mut acc, own, &Tier::Own)?;

    in_progress.pop();
    let result = acc.into_service_fields();
    cache.insert(cache_key, result.clone());
    Ok(result)
}

/// Resolves one `with`-list item: looks up its (possibly alias-qualified)
/// target template, validates its arguments against the target's
/// declared parameters (exact match — DESIGN.md's "fully applied at each
/// call, no partial application"), resolves the template itself — using
/// the target's *own* declaring scope, per [`SymbolResolver`]'s scoping
/// contract, not `scope` (the scope `inv` was written in) — then
/// substitutes every `Literal::Param` the resolution produced with the
/// bound concrete argument value.
fn resolve_invocation<R: SymbolResolver>(
    inv: &TemplateInvocation,
    scope: R::Scope,
    resolver: &R,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    in_progress: &mut Vec<(R::Scope, String)>,
    extra_networks: &mut Vec<Network>,
) -> Result<ServiceFields, ComposeError> {
    let (target_scope, decl) =
        resolver.resolve_template(scope, inv.qualifier.as_ref(), &inv.name.name, inv.span)?;

    let mut seen: HashMap<&str, Span> = HashMap::new();
    let mut args: HashMap<&str, &RawValue> = HashMap::new();
    for entry in &inv.args.entries {
        let key = entry.key.text();
        if let Some(&first_span) = seen.get(key) {
            return Err(ComposeError::DuplicateTemplateArgument {
                template: decl.name.name.clone(),
                argument: key.to_string(),
                first: first_span,
                second: entry.span,
            });
        }
        seen.insert(key, entry.span);
        if !decl.params.iter().any(|p| p.name == key) {
            return Err(ComposeError::UnknownTemplateArgument {
                template: decl.name.name.clone(),
                argument: key.to_string(),
                span: entry.span,
            });
        }
        args.insert(key, &entry.value);
    }
    for param in &decl.params {
        if !args.contains_key(param.name.as_str()) {
            return Err(ComposeError::MissingTemplateArgument {
                template: decl.name.name.clone(),
                param: param.name.clone(),
                span: inv.span,
            });
        }
    }

    let mut fields = resolve_template(
        decl,
        target_scope,
        resolver,
        cache,
        in_progress,
        extra_networks,
    )?;
    substitute_params(&mut fields, &args, &decl.name.name)?;
    Ok(fields)
}

/// Resolves every *qualified* `networks [alias.name]` entry in `fields`
/// against `scope` (rewriting it to an unqualified, resolved bare
/// reference so [`merge_tier`] never needs to know imports exist), and
/// rejects a qualified `middleware`/`depends_on` entry outright — see
/// [`ComposeError::UnsupportedQualifiedReference`]'s doc for why those
/// two have no cross-file meaning yet. Runs exactly once per scope, at
/// the point that scope's own directly-written fields are merged (its
/// `Tier::Own` step in [`compose_service`]/[`resolve_template`]) — by
/// induction, every `ServiceFields` [`merge_tier`] ever sees has already
/// passed through this, transitively, since a `with`-list target's own
/// qualified references were already resolved when *it* was resolved.
fn resolve_qualified_networks<R: SymbolResolver>(
    fields: &mut ServiceFields,
    scope: R::Scope,
    resolver: &R,
    extra_networks: &mut Vec<Network>,
) -> Result<(), ComposeError> {
    for r in &mut fields.networks {
        if let Some(qualifier) = r.qualifier.take() {
            let network = resolver.resolve_qualified_network(scope, &qualifier, &r.name, r.span)?;
            r.name = network.name.name.clone();
            extra_networks.push(network.clone());
        }
    }
    reject_qualified(&fields.middleware, "middleware")?;
    reject_qualified(&fields.depends_on, "depends_on")?;
    Ok(())
}

fn reject_qualified(refs: &[Reference], field: &'static str) -> Result<(), ComposeError> {
    for r in refs {
        if let Some(q) = &r.qualifier {
            return Err(ComposeError::UnsupportedQualifiedReference {
                field,
                alias: q.name.clone(),
                span: r.span,
            });
        }
    }
    Ok(())
}

/// Walks every `Literal`/`RawValue` slot in `fields` (mirroring
/// [`crate::parser`]'s parameter-marking walk) and replaces each
/// `Literal::Param` with the bound argument value in `args`.
fn substitute_params(
    fields: &mut ServiceFields,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
) -> Result<(), ComposeError> {
    if let Some(img) = &mut fields.image
        && let Some(r) = &mut img.reference
    {
        substitute_literal(r, args, template_name)?;
    }
    if let Some(e) = &mut fields.expose {
        if let Some(p) = &mut e.port {
            substitute_literal(p, args, template_name)?;
        }
        if let Some(h) = &mut e.host {
            substitute_literal(h, args, template_name)?;
        }
    }
    if let Some(r) = &mut fields.restart
        && let Some(p) = &mut r.policy
    {
        substitute_literal(p, args, template_name)?;
    }
    if let Some(cn) = &mut fields.container_name {
        substitute_literal(cn, args, template_name)?;
    }
    for v in &mut fields.volumes.entries {
        substitute_literal(&mut v.host, args, template_name)?;
        substitute_literal(&mut v.container, args, template_name)?;
    }
    for e in &mut fields.env.entries {
        substitute_literal(&mut e.key, args, template_name)?;
        substitute_literal(&mut e.value, args, template_name)?;
    }
    for entry in &mut fields.raw.entries {
        substitute_literal(&mut entry.key, args, template_name)?;
        substitute_raw_value(&mut entry.value, args);
    }
    for inv in &mut fields.with {
        for entry in &mut inv.args.entries {
            substitute_literal(&mut entry.key, args, template_name)?;
            substitute_raw_value(&mut entry.value, args);
        }
    }
    Ok(())
}

/// Substitutes a single `Literal` slot in place if it's a `Param`. A
/// plain `Literal`-typed slot can only ever hold one literal, so an
/// argument that resolves to a list/nested-map is a hard error here (see
/// [`ComposeError::TemplateArgumentNotScalar`]) — unlike
/// [`substitute_raw_value`], which can accept a full list/map forwarded
/// through a `with`-invocation's own argument body.
fn substitute_literal(
    lit: &mut Literal,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    let Some(name) = param_name else {
        return Ok(());
    };
    let span = lit.span();
    let replacement = args
        .get(name.as_str())
        .expect("param name was already validated against the template's declared params");
    match replacement {
        RawValue::Literal(actual) => {
            *lit = actual.clone();
            Ok(())
        }
        RawValue::List(_, _) | RawValue::Map(_, _) => {
            Err(ComposeError::TemplateArgumentNotScalar {
                template: template_name.to_string(),
                param: name,
                span,
            })
        }
    }
}

/// Substitutes every `Param` reachable inside a schema-free
/// [`RawValue`] tree (a `raw` entry's value, or a nested
/// `with`-invocation's own argument body) — recurses into lists/maps
/// since, unlike a plain `Literal` slot, a `RawValue` position can accept
/// a whole list/map forwarded through unchanged. Never fails (unlike
/// [`substitute_literal`]): a `RawValue` position can hold any argument
/// shape, so there's no "not scalar" case to reject here.
fn substitute_raw_value(value: &mut RawValue, args: &HashMap<&str, &RawValue>) {
    let param_name = match value {
        RawValue::Literal(Literal::Param(name, _)) => Some(name.clone()),
        _ => None,
    };
    if let Some(name) = param_name {
        let replacement = args
            .get(name.as_str())
            .expect("param name was already validated against the template's declared params");
        *value = (*replacement).clone();
        return;
    }
    match value {
        RawValue::List(items, _) => {
            for item in items {
                substitute_raw_value(item, args);
            }
        }
        RawValue::Map(entries, _) => {
            for (_, v) in entries {
                substitute_raw_value(v, args);
            }
        }
        RawValue::Literal(_) => {}
    }
}

// ---- merge engine ----
//
// Everything below here is completely unaware imports exist: it always
// operates on already-resolved `ServiceFields` (any qualified
// `networks` entry has already been rewritten to a plain resolved
// reference by `resolve_qualified_networks`, and a qualified
// `middleware`/`depends_on` entry can never reach here at all, having
// already been rejected). Kept byte-for-byte the same as before imports
// existed, deliberately, since it's the single largest, most-tested
// piece of this module.

/// Which priority tier a value came from, per docs/DESIGN.md's Composition
/// section: `Defaults` < `Explicit(template_name)` (left-to-right among
/// themselves) < `Own` (the service's/template's own body).
#[derive(Debug, Clone, PartialEq)]
enum Tier {
    Defaults,
    Explicit(String),
    Own,
}

trait Spanned {
    fn span(&self) -> Span;
}
impl Spanned for Image {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for Expose {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for Restart {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for VolumeEntry {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for EnvEntry {
    fn span(&self) -> Span {
        self.span
    }
}

/// The accumulator a field-bag's tiers merge into, tracking which tier
/// last set each field so [`merge_single`]/[`merge_map`] can tell
/// "explicit-vs-explicit" (an error) apart from "defaults-vs-anything"
/// or "anything-vs-own" (silent overrides).
#[derive(Default)]
struct MergeAcc {
    image: Option<(Image, Tier)>,
    expose: Option<(Expose, Tier)>,
    restart: Option<(Restart, Tier)>,
    container_name: Option<(Literal, Tier)>,
    volumes: Vec<(VolumeEntry, Tier)>,
    env: Vec<(EnvEntry, Tier)>,
    raw: RawMap,
    middleware: Vec<Reference>,
    depends_on: Vec<Reference>,
    networks: Vec<Reference>,
}

impl MergeAcc {
    fn into_service_fields(self) -> ServiceFields {
        ServiceFields {
            image: self.image.map(|(v, _)| v),
            expose: self.expose.map(|(v, _)| v),
            restart: self.restart.map(|(v, _)| v),
            container_name: self.container_name.map(|(v, _)| v),
            volumes: VolumeMap {
                entries: self.volumes.into_iter().map(|(v, _)| v).collect(),
            },
            env: EnvMap {
                entries: self.env.into_iter().map(|(v, _)| v).collect(),
            },
            raw: self.raw,
            middleware: self.middleware,
            depends_on: self.depends_on,
            networks: self.networks,
            with: Vec::new(),
        }
    }
}

/// Merges one tier's [`ServiceFields`] into `acc`. List fields
/// (`raw`/`middleware`/`depends_on`/`networks`) always concatenate — see
/// [`ComposeError::MapKeyCollision`]'s doc for why `raw` in particular is
/// never collision-checked, consistent with its existing intra-body
/// no-uniqueness behavior.
fn merge_tier(
    acc: &mut MergeAcc,
    incoming: ServiceFields,
    tier: &Tier,
) -> Result<(), ComposeError> {
    if let Some(img) = incoming.image {
        merge_single(&mut acc.image, "image", img, tier)?;
    }
    if let Some(e) = incoming.expose {
        merge_single(&mut acc.expose, "expose", e, tier)?;
    }
    if let Some(r) = incoming.restart {
        merge_single(&mut acc.restart, "restart", r, tier)?;
    }
    if let Some(cn) = incoming.container_name {
        merge_scalar_literal(&mut acc.container_name, "container_name", cn, tier)?;
    }
    merge_map(
        &mut acc.volumes,
        "volume",
        MapSide::Value,
        incoming.volumes.entries,
        tier,
        |e| e.container.text().to_string(),
    )?;
    merge_map(
        &mut acc.env,
        "env",
        MapSide::Key,
        incoming.env.entries,
        tier,
        |e| e.key.text().to_string(),
    )?;
    acc.raw.entries.extend(incoming.raw.entries);
    acc.middleware.extend(incoming.middleware);
    acc.depends_on.extend(incoming.depends_on);
    acc.networks.extend(incoming.networks);
    Ok(())
}

/// Merges one scalar/struct-kind field slot. `Own` always wins
/// unconditionally; `Defaults` is silently overridden by anything;
/// two `Explicit` tiers setting the same field is a compile error.
fn merge_single<T: Spanned>(
    slot: &mut Option<(T, Tier)>,
    field: &'static str,
    value: T,
    tier: &Tier,
) -> Result<(), ComposeError> {
    match slot.take() {
        None => {
            *slot = Some((value, tier.clone()));
        }
        Some((existing, existing_tier)) => match (&existing_tier, tier) {
            (_, Tier::Own) => {
                *slot = Some((value, Tier::Own));
            }
            (Tier::Defaults, _) => {
                *slot = Some((value, tier.clone()));
            }
            (Tier::Explicit(first), Tier::Explicit(second)) => {
                return Err(ComposeError::FieldCollision {
                    field,
                    first_template: first.clone(),
                    second_template: second.clone(),
                    first: existing.span(),
                    second: value.span(),
                });
            }
            _ => unreachable!("Own is always merged last; Defaults is always merged first"),
        },
    }
    Ok(())
}

/// Same tier rules as [`merge_single`], specialized to a bare [`Literal`]
/// slot (`container_name`) instead of a `Spanned`-bounded struct — kept
/// as its own small function rather than widening [`merge_single`]'s
/// bound, since [`Literal`] already carries its own inherent `span()`.
fn merge_scalar_literal(
    slot: &mut Option<(Literal, Tier)>,
    field: &'static str,
    value: Literal,
    tier: &Tier,
) -> Result<(), ComposeError> {
    match slot.take() {
        None => {
            *slot = Some((value, tier.clone()));
        }
        Some((existing, existing_tier)) => match (&existing_tier, tier) {
            (_, Tier::Own) => {
                *slot = Some((value, Tier::Own));
            }
            (Tier::Defaults, _) => {
                *slot = Some((value, tier.clone()));
            }
            (Tier::Explicit(first), Tier::Explicit(second)) => {
                return Err(ComposeError::FieldCollision {
                    field,
                    first_template: first.clone(),
                    second_template: second.clone(),
                    first: existing.span(),
                    second: value.span(),
                });
            }
            _ => unreachable!("Own is always merged last; Defaults is always merged first"),
        },
    }
    Ok(())
}

/// Merges one map-kind field's entries, keyed by `key_of` (the container
/// path for `volume`, the key for `env` — matching each field's existing
/// [`MapSide`] uniqueness convention). Same tier rules as
/// [`merge_single`], applied per-key rather than to the field as a whole.
fn merge_map<E: Spanned>(
    acc: &mut Vec<(E, Tier)>,
    field: &'static str,
    side: MapSide,
    incoming: Vec<E>,
    tier: &Tier,
    key_of: impl Fn(&E) -> String,
) -> Result<(), ComposeError> {
    for entry in incoming {
        let key = key_of(&entry);
        if let Some(pos) = acc.iter().position(|(e, _)| key_of(e) == key) {
            let existing_tier = acc[pos].1.clone();
            match (&existing_tier, tier) {
                (_, Tier::Own) => {
                    acc[pos] = (entry, Tier::Own);
                }
                (Tier::Defaults, _) => {
                    acc[pos] = (entry, tier.clone());
                }
                (Tier::Explicit(first), Tier::Explicit(second)) => {
                    let first_span = acc[pos].0.span();
                    return Err(ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
                        field,
                        side,
                        key,
                        first_template: first.clone(),
                        second_template: second.clone(),
                        first: first_span,
                        second: entry.span(),
                    })));
                }
                _ => unreachable!("Own is always merged last; Defaults is always merged first"),
            }
        } else {
            acc.push((entry, tier.clone()));
        }
    }
    Ok(())
}
