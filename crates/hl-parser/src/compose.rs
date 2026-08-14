//! Resolves `template`/`with` composition — the pass that runs on
//! [`crate::parse`]'s output to turn a service's own body plus whatever
//! templates it pulls in via `with` into one fully-merged
//! [`crate::Service`], per docs/DESIGN.md's "Composition" section.
//!
//! Kept as a separate pass from `parse()` deliberately: `parse()` stays
//! purely syntactic (known fields, correct kinds, no illegal
//! duplicates), while template resolution — a semantic concern, same as
//! "required fields" already being deferred past `parse()` — lives here.

use std::collections::HashMap;
use std::fmt;

use hl_lexer::Span;

use crate::ast::{
    EnvEntry, EnvMap, Expose, Image, Literal, Network, Program, RawMap, RawValue, Reference,
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
            | ComposeError::FieldCollision { second: span, .. } => *span,
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
        }
    }
}

impl std::error::Error for ComposeError {}

/// Resolves every `template`/`with` composition in `program`. Templates
/// are collected into a whole-program symbol table first (so `with` can
/// reference a template declared anywhere in the file, not just earlier
/// in it), then each service's `with`-list is merged per
/// docs/DESIGN.md's 3-tier priority: the implicit `defaults` template (if
/// declared) at the lowest tier, each explicit `with`-listed template
/// left-to-right above that (collisions between two of these are
/// errors), and the service's own body on top (always wins,
/// unconditionally). Resolution stops at the first error, matching
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
        }
    }

    let mut cache: HashMap<String, ServiceFields> = HashMap::new();
    let mut composed = Vec::with_capacity(services.len());
    for service in services {
        composed.push(compose_service(service, &templates, &mut cache)?);
    }

    Ok(ComposedProgram {
        networks,
        services: composed,
    })
}

fn compose_service(
    service: Service,
    templates: &HashMap<String, TemplateDecl>,
    cache: &mut HashMap<String, ServiceFields>,
) -> Result<Service, ComposeError> {
    let mut acc = MergeAcc::default();
    let mut in_progress = Vec::new();

    if let Some(defaults_decl) = templates.get("defaults") {
        let resolved = resolve_template(defaults_decl, templates, cache, &mut in_progress)?;
        merge_tier(&mut acc, resolved, &Tier::Defaults)?;
    }

    for inv in &service.fields.with {
        let resolved = resolve_invocation(inv, templates, cache, &mut in_progress)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }

    let mut own = service.fields;
    own.with.clear();
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
/// resolution). The result is cached by template name — still in
/// *parameterized* form (any `Literal::Param` the template's own body
/// declared is left untouched) — since the same template can be invoked
/// multiple times with different concrete arguments; substitution always
/// happens on a fresh clone in [`resolve_invocation`], never mutating the
/// cache.
fn resolve_template(
    decl: &TemplateDecl,
    templates: &HashMap<String, TemplateDecl>,
    cache: &mut HashMap<String, ServiceFields>,
    in_progress: &mut Vec<String>,
) -> Result<ServiceFields, ComposeError> {
    let name = &decl.name.name;
    if let Some(fields) = cache.get(name) {
        return Ok(fields.clone());
    }
    if in_progress.iter().any(|n| n == name) {
        let mut chain = in_progress.clone();
        chain.push(name.clone());
        return Err(ComposeError::TemplateCycle {
            chain,
            span: decl.name.span,
        });
    }
    in_progress.push(name.clone());

    let mut acc = MergeAcc::default();
    for inv in &decl.fields.with {
        let resolved = resolve_invocation(inv, templates, cache, in_progress)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }
    let mut own = decl.fields.clone();
    own.with.clear();
    merge_tier(&mut acc, own, &Tier::Own)?;

    in_progress.pop();
    let result = acc.into_service_fields();
    cache.insert(name.clone(), result.clone());
    Ok(result)
}

/// Resolves one `with`-list item: validates its arguments against the
/// target template's declared parameters (exact match — DESIGN.md's
/// "fully applied at each call, no partial application"), resolves the
/// template itself, then substitutes every `Literal::Param` the
/// resolution produced with the bound concrete argument value.
fn resolve_invocation(
    inv: &TemplateInvocation,
    templates: &HashMap<String, TemplateDecl>,
    cache: &mut HashMap<String, ServiceFields>,
    in_progress: &mut Vec<String>,
) -> Result<ServiceFields, ComposeError> {
    let decl = templates
        .get(&inv.name.name)
        .ok_or_else(|| ComposeError::UnknownTemplate {
            name: inv.name.name.clone(),
            span: inv.span,
        })?;

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

    let mut fields = resolve_template(decl, templates, cache, in_progress)?;
    substitute_params(&mut fields, &args, &decl.name.name)?;
    Ok(fields)
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
