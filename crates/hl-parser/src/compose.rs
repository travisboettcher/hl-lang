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
//! `SingleFileResolver`) and cross-file `alias.name` references (a
//! future linker, over its own module graph) with one implementation —
//! see [`SymbolResolver`]'s own doc for the scoping contract that makes
//! a template's own references always resolve in the scope it was
//! *declared* in, not the scope it's invoked from.

use std::collections::HashMap;
use std::fmt;

use hl_lexer::Span;

use crate::ast::{
    EnvEntry, EnvMap, Expose, Ident, Image, Literal, Network, ParamType, Program, RawMap, RawValue,
    Reference, Restart, Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl,
    VolumeEntry, VolumeMap,
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
    /// Two top-level `template` declarations share a name — template
    /// names are looked up by name, so they must be unique.
    DuplicateTemplateName {
        name: String,
        first: Span,
        second: Span,
    },
    /// Two top-level `service` declarations share a name. A service name
    /// becomes a Compose service key, which is unique by construction,
    /// so the second declaration used to silently swallow the first —
    /// including its Traefik labels — with nothing to indicate a whole
    /// service had gone missing (#63).
    DuplicateServiceName {
        name: String,
        first: Span,
        second: Span,
    },
    /// Two top-level `network` declarations share a name. Worse than the
    /// service case: the linker keeps declarations both in source order
    /// (first-wins for anything reading the list) and in a by-name map
    /// (last-wins for `alias.name` lookups), so a bare and a qualified
    /// reference to the same duplicated name could resolve to *different*
    /// declarations (#63).
    DuplicateNetworkName {
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
    /// A typed parameter (`port: Number`) was invoked with a scalar
    /// argument whose own literal kind doesn't match — strict, not
    /// coercive: `Number` rejects a quoted string even if it's
    /// numeric-looking, `String` rejects a bare number. Per
    /// docs/DESIGN.md: no implicit coercion between kinds.
    ArgumentTypeMismatch {
        template: String,
        param: String,
        expected: ParamType,
        found: &'static str,
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
    /// `SingleFileResolver`, which has no aliases at all: a lone
    /// [`Program`] has no imports by definition.
    UnknownAlias { alias: String, span: Span },
    /// A qualified reference (`alias.name`) was used on a reference-list
    /// field that has no cross-file meaning — `middleware`,
    /// `depends_on`, `dns`, or `expose.entrypoint`. (`depends_on` names
    /// a same-file sibling service; the others aren't resolved against
    /// anything an `.hll` file declares at all — an entry point lives
    /// in the deployment's own `traefik.yml`.) Rejected rather than
    /// silently accepted or silently dropped. Only `networks` resolves
    /// a qualifier, because a `network` really is a declaration another
    /// file can export.
    UnsupportedQualifiedReference {
        field: &'static str,
        alias: String,
        span: Span,
    },
    /// The implicit `defaults` template declares parameters. Every other
    /// template is reached through an explicit `with` invocation, which
    /// is where arguments are bound; `defaults` has no invocation at all
    /// (it's applied to every service automatically), so there is
    /// nowhere for a caller to supply one and nothing to substitute its
    /// `$param` references with.
    ParameterizedDefaults { param: String, span: Span },
    /// A qualified `networks [alias.name]` entry's `alias` resolved to a
    /// real imported scope, but no `network` named `name` exists there.
    /// Distinct from [`Self::UnknownAlias`] (the alias itself didn't
    /// resolve) — `SingleFileResolver` never raises this, since every
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
            | ComposeError::DuplicateServiceName { second: span, .. }
            | ComposeError::DuplicateNetworkName { second: span, .. }
            | ComposeError::TemplateCycle { span, .. }
            | ComposeError::UnknownTemplateArgument { span, .. }
            | ComposeError::MissingTemplateArgument { span, .. }
            | ComposeError::DuplicateTemplateArgument { second: span, .. }
            | ComposeError::TemplateArgumentNotScalar { span, .. }
            | ComposeError::ArgumentTypeMismatch { span, .. }
            | ComposeError::FieldCollision { second: span, .. }
            | ComposeError::UnknownAlias { span, .. }
            | ComposeError::UnsupportedQualifiedReference { span, .. }
            | ComposeError::ParameterizedDefaults { span, .. }
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
            ComposeError::DuplicateServiceName { name, first, .. } => write!(
                f,
                "{}:{}: duplicate service `{name}` (first declared at {}:{})",
                span.line, span.col, first.line, first.col
            ),
            ComposeError::DuplicateNetworkName { name, first, .. } => write!(
                f,
                "{}:{}: duplicate network `{name}` (first declared at {}:{})",
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
            ComposeError::ArgumentTypeMismatch {
                template,
                param,
                expected,
                found,
                ..
            } => write!(
                f,
                "{}:{}: argument `{param}` for template `{template}` must be a {expected} (found {found})",
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
            ComposeError::ParameterizedDefaults { param, .. } => write!(
                f,
                "{}:{}: template `defaults` must not declare parameters (`{param}`) — it's applied implicitly to every service, so there's no call site to bind them",
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

#[cfg(test)]
mod error_display_tests {
    use super::*;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
        }
    }

    #[test]
    fn unknown_template_display() {
        let err = ComposeError::UnknownTemplate {
            name: "base".to_string(),
            span: span(3, 2),
        };
        assert_eq!(err.to_string(), "3:2: unknown template `base`");
    }

    #[test]
    fn duplicate_template_name_display() {
        let err = ComposeError::DuplicateTemplateName {
            name: "base".to_string(),
            first: span(1, 1),
            second: span(4, 1),
        };
        assert_eq!(
            err.to_string(),
            "4:1: duplicate template `base` (first declared at 1:1)"
        );
    }

    #[test]
    fn duplicate_service_name_display() {
        let err = ComposeError::DuplicateServiceName {
            name: "web".to_string(),
            first: span(1, 1),
            second: span(6, 1),
        };
        assert_eq!(
            err.to_string(),
            "6:1: duplicate service `web` (first declared at 1:1)"
        );
    }

    #[test]
    fn duplicate_network_name_display() {
        let err = ComposeError::DuplicateNetworkName {
            name: "proxy".to_string(),
            first: span(2, 1),
            second: span(9, 1),
        };
        assert_eq!(
            err.to_string(),
            "9:1: duplicate network `proxy` (first declared at 2:1)"
        );
    }

    #[test]
    fn template_cycle_display() {
        let err = ComposeError::TemplateCycle {
            chain: vec!["a".to_string(), "b".to_string(), "a".to_string()],
            span: span(2, 3),
        };
        assert_eq!(
            err.to_string(),
            "2:3: template composition cycle: a -> b -> a"
        );
    }

    #[test]
    fn unknown_template_argument_display() {
        let err = ComposeError::UnknownTemplateArgument {
            template: "t".to_string(),
            argument: "bogus".to_string(),
            span: span(5, 1),
        };
        assert_eq!(
            err.to_string(),
            "5:1: unknown argument `bogus` for template `t`"
        );
    }

    #[test]
    fn missing_template_argument_display() {
        let err = ComposeError::MissingTemplateArgument {
            template: "t".to_string(),
            param: "name".to_string(),
            span: span(6, 1),
        };
        assert_eq!(
            err.to_string(),
            "6:1: missing required argument `name` for template `t`"
        );
    }

    #[test]
    fn duplicate_template_argument_display() {
        let err = ComposeError::DuplicateTemplateArgument {
            template: "t".to_string(),
            argument: "name".to_string(),
            first: span(1, 5),
            second: span(1, 10),
        };
        assert_eq!(
            err.to_string(),
            "1:10: duplicate argument `name` for template `t` (first set at 1:5)"
        );
    }

    #[test]
    fn template_argument_not_scalar_display() {
        let err = ComposeError::TemplateArgumentNotScalar {
            template: "t".to_string(),
            param: "name".to_string(),
            span: span(2, 2),
        };
        assert_eq!(
            err.to_string(),
            "2:2: argument `name` for template `t` must be a scalar value (a list/map can't fill a single-value field)"
        );
    }

    #[test]
    fn argument_type_mismatch_display() {
        let err = ComposeError::ArgumentTypeMismatch {
            template: "t".to_string(),
            param: "port".to_string(),
            expected: ParamType::Number,
            found: "a quoted string",
            span: span(2, 2),
        };
        assert_eq!(
            err.to_string(),
            "2:2: argument `port` for template `t` must be a Number (found a quoted string)"
        );
    }

    #[test]
    fn field_collision_display() {
        let err = ComposeError::FieldCollision {
            field: "image",
            first_template: "a".to_string(),
            second_template: "b".to_string(),
            first: span(1, 1),
            second: span(2, 1),
        };
        assert_eq!(
            err.to_string(),
            "2:1: field `image` set by both template `a` (at 1:1) and template `b` — explicit templates must not conflict"
        );
    }

    #[test]
    fn map_key_collision_display_key_side() {
        let err = ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
            field: "env",
            side: MapSide::Key,
            key: "FOO".to_string(),
            first_template: "a".to_string(),
            second_template: "b".to_string(),
            first: span(1, 1),
            second: span(2, 1),
        }));
        assert_eq!(
            err.to_string(),
            "2:1: `env` key \"FOO\" set by both template `a` (at 1:1) and template `b` — explicit templates must not conflict"
        );
    }

    #[test]
    fn map_key_collision_display_value_side() {
        let err = ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
            field: "volume",
            side: MapSide::Value,
            key: "/data".to_string(),
            first_template: "a".to_string(),
            second_template: "b".to_string(),
            first: span(1, 1),
            second: span(2, 1),
        }));
        assert_eq!(
            err.to_string(),
            "2:1: `volume` value \"/data\" set by both template `a` (at 1:1) and template `b` — explicit templates must not conflict"
        );
    }

    #[test]
    fn unknown_alias_display() {
        let err = ComposeError::UnknownAlias {
            alias: "traefik".to_string(),
            span: span(1, 3),
        };
        assert_eq!(err.to_string(), "1:3: unknown alias `traefik`");
    }

    #[test]
    fn unsupported_qualified_reference_display() {
        let err = ComposeError::UnsupportedQualifiedReference {
            field: "middleware",
            alias: "traefik".to_string(),
            span: span(1, 3),
        }
        .to_string();
        assert_eq!(
            err,
            "1:3: `middleware` doesn't support a qualified reference yet (`traefik.` ...)"
        );
    }

    #[test]
    fn parameterized_defaults_display() {
        let err = ComposeError::ParameterizedDefaults {
            param: "x".to_string(),
            span: span(1, 18),
        };
        assert_eq!(
            err.to_string(),
            "1:18: template `defaults` must not declare parameters (`x`) — it's applied implicitly to every service, so there's no call site to bind them"
        );
    }

    #[test]
    fn unknown_qualified_network_display() {
        let err = ComposeError::UnknownQualifiedNetwork {
            alias: "traefik".to_string(),
            name: "proxy".to_string(),
            span: span(2, 2),
        };
        assert_eq!(err.to_string(), "2:2: no network `proxy` in `traefik`");
    }
}

/// Resolves names against a whole-program symbol table, generalized over
/// an opaque `Scope` so the same merge engine ([`compose_with_resolver`])
/// works both for a single already-parsed [`Program`] (via
/// `SingleFileResolver`, `Scope = ()`) and, in a future milestone, a
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
/// followed (see `SingleFileResolver`'s doc: a `use` declaration
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
    // Networks and services are kept as ordered `Vec`s (source order is
    // load-bearing downstream), so unlike templates they need their own
    // by-name tables purely to detect a redeclaration.
    let mut network_spans: HashMap<String, Span> = HashMap::new();
    let mut service_spans: HashMap<String, Span> = HashMap::new();

    for decl in program.decls {
        match decl {
            TopDecl::Network(n) => {
                if let Some(&first) = network_spans.get(&n.name.name) {
                    return Err(ComposeError::DuplicateNetworkName {
                        name: n.name.name.clone(),
                        first,
                        second: n.name.span,
                    });
                }
                network_spans.insert(n.name.name.clone(), n.name.span);
                networks.push(n);
            }
            TopDecl::Service(s) => {
                if let Some(&first) = service_spans.get(&s.name.name) {
                    return Err(ComposeError::DuplicateServiceName {
                        name: s.name.name.clone(),
                        first,
                        second: s.name.span,
                    });
                }
                service_spans.insert(s.name.name.clone(), s.name.span);
                services.push(*s);
            }
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
        // `defaults` is the one template applied without an invocation,
        // so it reaches `resolve_template` directly rather than through
        // `resolve_invocation` — which is where arity is checked and
        // where `Literal::Param`s are substituted. A parameterized
        // `defaults` would therefore skip both: its `$x` references
        // would survive composition, reaching codegen either as the
        // parameter's own *name* emitted as a real Compose value or as
        // a hard error deep in transcription (#62). Rejecting it up
        // front is the only coherent answer — there is no call site
        // that could ever supply the arguments.
        if let Some(param) = defaults_decl.params.first() {
            return Err(ComposeError::ParameterizedDefaults {
                param: param.name.name.clone(),
                span: param.name.span,
            });
        }
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
        if !decl.params.iter().any(|p| p.name.name == key) {
            return Err(ComposeError::UnknownTemplateArgument {
                template: decl.name.name.clone(),
                argument: key.to_string(),
                span: entry.span,
            });
        }
        args.insert(key, &entry.value);
    }
    for param in &decl.params {
        match args.get(param.name.name.as_str()) {
            None => {
                return Err(ComposeError::MissingTemplateArgument {
                    template: decl.name.name.clone(),
                    param: param.name.name.clone(),
                    span: inv.span,
                });
            }
            Some(value) => {
                if let Some(expected) = param.ty {
                    check_argument_type(value, expected, &decl.name.name, &param.name.name)?;
                }
            }
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
/// rejects a qualified `middleware`/`depends_on`/`dns`/
/// `expose.entrypoint` entry outright — see
/// [`ComposeError::UnsupportedQualifiedReference`]'s doc for why those
/// have no cross-file meaning yet. Runs exactly once per scope, at
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
    reject_qualified(&fields.dns, "dns")?;
    // `expose.entrypoint` joined this list when it became a reference
    // list. An entry point names something in the deployment's own
    // `traefik.yml`, which no `.hll` file declares, so there is nothing
    // for an alias to resolve against — and codegen reads only
    // `Reference::name`, so an unchecked `traefik.web` would compile
    // happily to `entrypoints=web` with the qualifier silently gone.
    if let Some(expose) = &fields.expose {
        reject_qualified(&expose.entrypoint, "expose.entrypoint")?;
    }
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

/// Checks a typed parameter's bound argument against its declared type —
/// strict, not coercive (see [`ComposeError::ArgumentTypeMismatch`]'s
/// doc): the argument's own literal kind must match `expected` exactly.
/// A list/map argument is left alone here — that's
/// [`ComposeError::TemplateArgumentNotScalar`]'s job, raised only if the
/// parameter is actually substituted into a scalar `Literal` slot,
/// independent of whether it's typed at all.
///
/// A still-unresolved `Literal::Param` (an outer template forwarding its
/// *own* parameter into this argument, e.g. `with inner { y: $x }` inside
/// `template outer(x) { ... }`) is also left unchecked: its concrete
/// literal kind isn't known until `$x` itself is substituted, which
/// happens only once `outer` is invoked with a real argument — and *that*
/// call site is where `x`'s own declared type (if any) gets checked
/// instead. A mismatch between `inner`'s declared type for `y` and
/// `outer`'s declared type for `x` (if the two disagree) isn't caught by
/// this pass — full type propagation through forwarding chains is beyond
/// this milestone's scope.
fn check_argument_type(
    value: &RawValue,
    expected: ParamType,
    template_name: &str,
    param_name: &str,
) -> Result<(), ComposeError> {
    let RawValue::Literal(lit) = value else {
        return Ok(());
    };
    let found = match lit {
        Literal::Number { .. } if expected == ParamType::Number => return Ok(()),
        Literal::Str(_, _) if expected == ParamType::String => return Ok(()),
        Literal::Param(_, _) => return Ok(()),
        Literal::Str(_, _) => "a quoted string",
        Literal::Number { .. } => "a number",
        Literal::Ident(_, _) => "a bare identifier",
    };
    Err(ComposeError::ArgumentTypeMismatch {
        template: template_name.to_string(),
        param: param_name.to_string(),
        expected,
        found,
        span: lit.span(),
    })
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
/// last set each value so [`merge_scalar`]/[`merge_map`] can tell
/// "explicit-vs-explicit" (an error) apart from "defaults-vs-anything"
/// or "anything-vs-own" (silent overrides).
///
/// `scalars` holds every single-value collision point in the language —
/// `image.ref`, `expose.port`/`expose.host`, `restart.policy`,
/// `container_name`, and any future one — keyed
/// generically by name, always the fully-dotted canonical path down to
/// the concrete sub-field (`expose`'s two scalar sub-fields; `image`/
/// `restart`'s one apiece), never a struct's own bare name, so a field's
/// key is a stable function of its own identity rather than how many
/// siblings its struct happens to have today (see #27: keying a
/// single-field struct under its bare name meant the key would have to
/// change out from under `image.ref`/`restart.policy` the moment either
/// struct grew a second field). A bare field with no enclosing struct at
/// all, like `container_name`, is keyed under its own name — there's no
/// sub-field path to be dotted onto. Which canonical keys exist, and how
/// each one's `Literal` slot is read out of/written back into
/// `ServiceFields`, is entirely described by the [`SCALAR_FIELDS`] table
/// below — see its doc for why that's what makes a new scalar collision
/// point a one-line addition rather than a new `MergeAcc` field plus new
/// hand-written merge/rebuild logic.
///
/// `lists` is the same idea for every reference-list field — the four
/// bare ones on `ServiceFields` (`middleware`/`depends_on`/`networks`/
/// `dns`) plus `expose.entrypoint`, which lives inside a nested struct
/// and so can't be a plain `MergeAcc` field the way the others once
/// were. They carry no `Tier`: list fields concatenate unconditionally,
/// so there is no collision to attribute to a tier. See [`LIST_FIELDS`].
#[derive(Default)]
struct MergeAcc {
    scalars: HashMap<&'static str, (Literal, Tier)>,
    lists: HashMap<&'static str, Vec<Reference>>,
    volumes: Vec<(VolumeEntry, Tier)>,
    env: Vec<(EnvEntry, Tier)>,
    raw: RawMap,
}

impl MergeAcc {
    fn into_service_fields(mut self) -> ServiceFields {
        let mut fields = ServiceFields {
            volumes: VolumeMap {
                entries: self.volumes.into_iter().map(|(v, _)| v).collect(),
            },
            env: EnvMap {
                entries: self.env.into_iter().map(|(v, _)| v).collect(),
            },
            raw: self.raw,
            ..Default::default()
        };
        for field in SCALAR_FIELDS {
            if let Some((value, _)) = self.scalars.remove(field.key) {
                (field.set)(&mut fields, value);
            }
        }
        // Deliberately after `SCALAR_FIELDS`: `expose.entrypoint`'s
        // `set` can create the `Expose` that holds it, and the span it
        // stamps on a freshly created one is meant to lose to
        // `expose.port`'s and `expose.host`'s (see [`ScalarField`]'s
        // doc). Running the whole scalar table first keeps
        // `entrypoint` last in that preference order, exactly where it
        // sat when it was the table's third `expose` row.
        for field in LIST_FIELDS {
            if let Some(values) = self.lists.remove(field.key) {
                (field.set)(&mut fields, values);
            }
        }
        fields
    }
}

/// One scalar collision point in `ServiceFields` — a `Literal` slot that
/// lives either directly on `ServiceFields` (`container_name`) or inside
/// one of its `Nested` struct fields (`image.ref`,
/// `expose.port`/`.host`, `restart.policy`) — described
/// generically by `key` (the identity-stable, fully-dotted name
/// [`merge_scalar`]/`ComposeError` key collisions by — see #27) plus a
/// pair of function pointers for reading the slot out of a tier's
/// `ServiceFields` (`take`) and writing a merged value back into a
/// freshly rebuilt one (`set`). This table is what lets [`merge_tier`]
/// and [`MergeAcc::into_service_fields`] each be one generic loop
/// instead of the two bespoke, hand-enumerated functions they used to
/// be (see hl-lang#28) — the only place left that needs to know
/// `ServiceFields`'s concrete struct shape. Adding a future scalar
/// collision point (a new struct sub-field, or a new bare scalar field)
/// means adding one `ScalarField` entry here, not touching either
/// generic function.
///
/// `expose`'s entries are listed `port` then `host` in that order
/// deliberately: `set`'s `get_or_insert` only stamps a freshly created
/// `Expose`'s span from the *first* sub-field processed that's actually
/// present, so this order reproduces the same port-over-host span
/// preference the old hand-written `into_service_fields` documented.
/// `expose.entrypoint` is no longer a row here — it's a reference list
/// now, so it lives in [`LIST_FIELDS`] — but it still sorts *after*
/// both of these for span purposes, because
/// [`MergeAcc::into_service_fields`] runs this whole table before that
/// one (the span itself stays cosmetic — see [`Expose`]'s doc —
/// nothing downstream reads it for anything but existence).
struct ScalarField {
    key: &'static str,
    take: fn(&mut ServiceFields) -> Option<Literal>,
    set: fn(&mut ServiceFields, Literal),
}

static SCALAR_FIELDS: &[ScalarField] = &[
    ScalarField {
        key: "image.ref",
        take: |f| f.image.take().and_then(|i| i.reference),
        set: |f, v| {
            f.image = Some(Image {
                span: v.span(),
                reference: Some(v),
            });
        },
    },
    ScalarField {
        key: "expose.port",
        take: |f| f.expose.as_mut().and_then(|e| e.port.take()),
        set: |f, v| {
            let span = v.span();
            f.expose
                .get_or_insert(Expose {
                    port: None,
                    host: None,
                    entrypoint: Vec::new(),
                    span,
                })
                .port = Some(v);
        },
    },
    ScalarField {
        key: "expose.host",
        take: |f| f.expose.as_mut().and_then(|e| e.host.take()),
        set: |f, v| {
            let span = v.span();
            f.expose
                .get_or_insert(Expose {
                    port: None,
                    host: None,
                    entrypoint: Vec::new(),
                    span,
                })
                .host = Some(v);
        },
    },
    ScalarField {
        key: "restart.policy",
        take: |f| f.restart.take().and_then(|r| r.policy),
        set: |f, v| {
            f.restart = Some(Restart {
                span: v.span(),
                policy: Some(v),
            });
        },
    },
    ScalarField {
        key: "container_name",
        take: |f| f.container_name.take(),
        set: |f, v| f.container_name = Some(v),
    },
];

/// [`ScalarField`]'s counterpart for reference-list fields — the same
/// `key`/`take`/`set` triple, minus everything scalars need only
/// because they can collide. A list never collides (tiers concatenate,
/// per docs/DESIGN.md's composition rules), so there's no `Tier` to
/// track and no error to key by name; `key` is here purely as the
/// `MergeAcc::lists` map key, and stays the same fully-dotted canonical
/// path convention `ScalarField::key` documents. `set` is only ever
/// called with a non-empty list — see [`merge_tier`].
///
/// Introduced when `expose.entrypoint` became a list (hl-lang#73).
/// Before that, every reference list sat directly on `ServiceFields`,
/// so [`merge_tier`] and [`MergeAcc::into_service_fields`] could just
/// name each one — but `entrypoint` lives inside `expose`, which
/// `into_service_fields` *rebuilds*, so it needs the same read-out/
/// write-back indirection the scalars already had. Given one list field
/// had to be described by function pointers, all five are: the whole
/// point of [`SCALAR_FIELDS`] (see hl-lang#28) is that both merge
/// functions stay one generic loop apiece with no hand-enumerated
/// knowledge of `ServiceFields`'s shape, and leaving four lists
/// hand-named beside a one-row table would have kept exactly the shape
/// that design set out to remove.
struct ListField {
    key: &'static str,
    take: fn(&mut ServiceFields) -> Vec<Reference>,
    set: fn(&mut ServiceFields, Vec<Reference>),
    /// Whether repeats of an already-accumulated name are dropped
    /// rather than appended (hl-lang#69). True for the set-like fields
    /// — `networks`, `middleware`, `depends_on`, `expose.entrypoint` —
    /// where naming the same thing twice means exactly what naming it
    /// once means, so the repeat is pure noise: it duplicated
    /// `networks:` entries and `middlewares=` label values in the
    /// output, made a single external network look like an ambiguity
    /// with itself, and (since list size then doubled per composition
    /// level) turned a few hundred bytes of nested `with` into an
    /// out-of-memory abort.
    ///
    /// `dns` is the one exception, deliberately: order is observable
    /// there (resolver priority), so its append semantics are left
    /// exactly as they were even though a repeat is just as
    /// meaningless. Every other reference list — `expose.entrypoint`
    /// included — is a set, and a router attached twice to the same
    /// entry point is attached to it once.
    dedupe: bool,
}

static LIST_FIELDS: &[ListField] = &[
    ListField {
        key: "expose.entrypoint",
        dedupe: true,
        take: |f| {
            f.expose
                .as_mut()
                .map(|e| std::mem::take(&mut e.entrypoint))
                .unwrap_or_default()
        },
        set: |f, v| {
            // `v` is never empty — see [`merge_tier`]'s `acc.lists`
            // invariant — so `v[0]` is safe, and the span it stamps on
            // a freshly created `Expose` is the first entry point's:
            // cosmetic, and only ever reached when neither `port` nor
            // `host` was set at all.
            let span = v[0].span;
            f.expose
                .get_or_insert(Expose {
                    port: None,
                    host: None,
                    entrypoint: Vec::new(),
                    span,
                })
                .entrypoint = v;
        },
    },
    ListField {
        key: "middleware",
        dedupe: true,
        take: |f| std::mem::take(&mut f.middleware),
        set: |f, v| f.middleware = v,
    },
    ListField {
        key: "depends_on",
        dedupe: true,
        take: |f| std::mem::take(&mut f.depends_on),
        set: |f, v| f.depends_on = v,
    },
    ListField {
        key: "networks",
        dedupe: true,
        take: |f| std::mem::take(&mut f.networks),
        set: |f, v| f.networks = v,
    },
    ListField {
        key: "dns",
        dedupe: false,
        take: |f| std::mem::take(&mut f.dns),
        set: |f, v| f.dns = v,
    },
];

/// Merges one tier's [`ServiceFields`] into `acc`. List fields (`raw`,
/// plus every [`LIST_FIELDS`] entry) concatenate rather than collide —
/// see [`ComposeError::MapKeyCollision`]'s doc for why `raw` in
/// particular is never collision-checked, consistent with its existing
/// intra-body no-uniqueness behavior. The set-like reference lists
/// concatenate *by distinct name*, dropping a repeat of a name an
/// earlier tier (or an earlier entry of the same list) already
/// contributed; see [`ListField::dedupe`] for which fields those are
/// and why `dns` isn't one of them.
fn merge_tier(
    acc: &mut MergeAcc,
    mut incoming: ServiceFields,
    tier: &Tier,
) -> Result<(), ComposeError> {
    for field in SCALAR_FIELDS {
        if let Some(value) = (field.take)(&mut incoming) {
            merge_scalar(&mut acc.scalars, field.key, value, tier)?;
        }
    }
    // Before the `merge_map` calls below only because those consume
    // `incoming`'s map entries by value, and `take` needs `incoming`
    // whole; the merge itself is order-independent.
    //
    // The emptiness check is load-bearing, not a micro-optimization:
    // it's what establishes `acc.lists`'s invariant that a key present
    // in the map always maps to a non-empty list. `into_service_fields`
    // relies on it (an empty list must round-trip as "field unset", not
    // as an `Expose` materialized to hold nothing), and
    // [`LIST_FIELDS`]'s `expose.entrypoint` `set` relies on it harder
    // still — it reads `v[0]` for a span. Deduping can't undermine
    // that: a non-empty `values` whose every entry is dropped as a
    // repeat can only happen when the accumulated list already held
    // those names, i.e. was already non-empty.
    for field in LIST_FIELDS {
        let values = (field.take)(&mut incoming);
        if !values.is_empty() {
            let acc_values = acc.lists.entry(field.key).or_default();
            if field.dedupe {
                // First occurrence wins, so the accumulated order is
                // still tier order (`defaults`, then each `with` target
                // left-to-right, then the body's own list) with later
                // repeats dropped — see [`ListField::dedupe`]. The
                // linear scan is over a list whose length is now bounded
                // by the number of *distinct* names, which is what makes
                // this cheap and is the whole reason #69's exponential
                // blowup stops here.
                //
                // Comparing by `Reference::name` alone is right because
                // a qualified `networks [alias.name]` entry has already
                // been rewritten to its resolved bare name by
                // [`resolve_qualified_networks`] before any tier reaches
                // this function, and the other deduped fields reject
                // qualifiers outright.
                for value in values {
                    if !acc_values.iter().any(|held| held.name == value.name) {
                        acc_values.push(value);
                    }
                }
            } else {
                acc_values.extend(values);
            }
        }
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
    Ok(())
}

/// Merges one scalar collision point, keyed by `field` (e.g.
/// `"expose.host"`), into `acc`. `Own` always wins unconditionally;
/// `Defaults` is silently overridden by anything; two `Explicit` tiers
/// setting the same key is a compile error. The single merge routine
/// every scalar field in the language goes through — see [`MergeAcc`]'s
/// own doc for why this replaced the old `Spanned`-generic, one-slot-
/// per-field `merge_single`.
fn merge_scalar(
    acc: &mut HashMap<&'static str, (Literal, Tier)>,
    field: &'static str,
    value: Literal,
    tier: &Tier,
) -> Result<(), ComposeError> {
    match acc.remove(field) {
        None => {
            acc.insert(field, (value, tier.clone()));
        }
        Some((existing, existing_tier)) => match (&existing_tier, tier) {
            (_, Tier::Own) => {
                acc.insert(field, (value, Tier::Own));
            }
            (Tier::Defaults, _) => {
                acc.insert(field, (value, tier.clone()));
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
