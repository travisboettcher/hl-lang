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

use hl_lexer::{SourceMap, Span};

use crate::ast::{
    ArrowMap, ArrowMapEntry, ArrowMapHost, Build, Command, DependsOnEntry, Entrypoint, EnvEntry,
    EnvMap, Expose, Healthcheck, HealthcheckTest, Ident, Image, LabelEntry, LabelMap, Literal,
    MatchExpr, Network, Program, RawEntry, RawMap, RawValue, Restart, Router, Service,
    ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, Traefik, Volume,
};
use crate::schema::{self, MapSide};

/// How deep a chain of `with` invocations may nest before
/// [`ComposeError::TemplateNestingTooDeep`] stops it.
///
/// Template resolution and invocation resolution are mutually
/// recursive, one level per `with` hop. The cycle check bounds
/// *repetition* — a template that reaches itself — but a plain chain
/// `template t1 { with t0 }`, `template t2 { with t1 }`, ... repeats
/// nothing, so it passed that check at every level and recursed until
/// the stack overflowed. A stack overflow aborts the process rather than
/// returning an error, which an embedder calling `parse()`/`link()` has
/// no way to defend against (#72).
///
/// Same reasoning as [`crate::MAX_RAW_VALUE_DEPTH`], but a much smaller
/// number, because these frames are far fatter than the parser's: on a
/// spawned thread's default 2 MiB stack — the floor that matters, since
/// an embedder may call `link()` off the main thread — a debug build
/// resolves 128 levels but aborts at 192, so a "few hundred" ceiling
/// would still overflow. 64 leaves roughly 4× headroom (it survives even
/// a 512 KiB stack) and is still far beyond any chain anyone would write
/// on purpose.
///
/// This bounds `with` *depth*, not breadth: a service or template can
/// still list as many templates side by side as it likes.
pub const MAX_TEMPLATE_DEPTH: usize = 64;

/// The result of resolving every `template`/`with` composition in a
/// [`Program`] — see [`compose`]. Every `Service` here has an empty
/// `fields.with` and contains no [`Literal::Param`] anywhere; producing
/// that is this module's whole job.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposedProgram {
    pub networks: Vec<Network>,
    /// The program's top-level `volume` declarations — the entry file's
    /// own first, in source order, then any an `alias.name` mount
    /// imported — and what codegen resolves each service's named-volume
    /// mount against, exactly as it resolves `networks [...]` against
    /// `networks` above.
    pub volumes: Vec<Volume>,
    pub services: Vec<Service>,
    /// Resolves the [`hl_lexer::FileId`] on any [`Span`] reachable from
    /// this program back to the file it came from, so a codegen
    /// diagnostic can name that file (#75).
    ///
    /// Empty for [`compose`], which is handed one already-parsed
    /// [`Program`] and never learns where it came from; `hl_linker`'s
    /// `link` fills it in with the module graph's own map, since that's
    /// the layer that actually reads the files.
    pub files: SourceMap,
}

/// An error raised while resolving `template`/`with` composition.
/// Mirrors [`crate::ParseError`]'s design (structured, span-carrying, no
/// error recovery — resolution stops at the first error).
#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    /// `with X` names a template with no
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
    /// Two top-level `volume` declarations share a name. Same reasoning
    /// as [`Self::DuplicateNetworkName`]: a named volume is resolved by
    /// its bare name, so two declarations under one name leave every
    /// reference to it ambiguous — and, since a volume declaration is
    /// what says whether the volume is `external` or carries a `name:`
    /// override, silently picking one would silently pick a different
    /// underlying Docker volume.
    DuplicateVolumeName {
        name: String,
        first: Span,
        second: Span,
    },
    /// A `with`-reference chain returns to a template already being
    /// resolved (e.g. `template a { with b }` / `template b { with a }`).
    /// `chain` lists the template names in resolution order, ending with
    /// the name that closes the cycle.
    TemplateCycle { chain: Vec<String>, span: Span },
    /// A `with`-reference chain nested deeper than
    /// [`MAX_TEMPLATE_DEPTH`] without repeating a template — so
    /// [`Self::TemplateCycle`] never fires, and resolution used to
    /// recurse until the stack gave out. Unlike a cycle, there's no
    /// chain worth printing — a list of every template on the way down
    /// is noise, not a diagnostic — so this reports the limit and the
    /// template that hit it.
    TemplateNestingTooDeep {
        name: String,
        limit: usize,
        span: Span,
    },
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
    /// A `$param` substituted into a reference-shaped position
    /// (`networks`, `dns`, `env_file`, a `depends_on`
    /// entry's own reference, `expose.entrypoint`, `router.entrypoints`,
    /// `router.path_prefix`, `router.middleware`) resolved to a bare
    /// number. #201 dropped
    /// `: Number`/`: String` parameter annotations in favor of checking a
    /// substituted argument against the field it actually lands in —
    /// see docs/DESIGN.md's Syntactic grammar section for the full
    /// reasoning. This is the one shape check that survives: a
    /// reference-shaped position's own grammar
    /// (`parser::Parser::parse_literal_reference`) can never produce
    /// [`Literal::Number`] directly, so one reaching here can only mean a
    /// template caller passed a bare number where a reference belongs.
    /// `span` names the *argument*, not the `$param` use site — see
    /// `substitute_reference_literal`'s own doc for why substitution
    /// leaves that span behind for free.
    ArgumentNotReferenceShaped {
        template: String,
        param: String,
        span: Span,
    },
    /// A `$param` substituted into one of the handful of positions
    /// `book/src/built-in-fields.md` documents as taking `number` —
    /// `expose.port`, `healthcheck.retries` — resolved to something
    /// other than a bare number. The companion check to
    /// [`Self::ArgumentNotReferenceShaped`]: dropping `: Number`/`:
    /// String` annotations (#201) meant a numeric field lost its
    /// declaration-site check exactly the way a reference-shaped one
    /// did, and gets the same substitution-time replacement here, for
    /// the same reason — see `substitute_numeric_literal`'s own doc.
    /// `span` names the argument, not the `$param` use site, for the
    /// same reason [`Self::ArgumentNotReferenceShaped`]'s does.
    ///
    /// [`Self::FieldNotNumeric`] is this check's backstop for a
    /// non-numeric literal that never passed through a `$param` at all —
    /// see that variant's own doc for why one check alone can't cover
    /// both paths.
    ArgumentNotNumeric {
        template: String,
        param: String,
        found: &'static str,
        span: Span,
    },
    /// The same mismatch [`Self::ArgumentNotNumeric`] rejects, caught
    /// the other way it can happen: a non-numeric `expose.port` or
    /// `healthcheck.retries` written directly — by a plain service, or
    /// inside a template's own body with no `$param` in sight — rather
    /// than arriving through a substituted argument.
    /// `substitute_numeric_literal` only ever looks at a
    /// [`crate::ast::Literal::Param`] slot, so a hand-written mismatch
    /// passes through it untouched; this is the check that still catches
    /// it, run once on each service's fully merged fields so it sees
    /// exactly what codegen would have. Since it runs after every
    /// `$param` in scope has already resolved, there's no template or
    /// parameter left to name — only the field, which is what it names
    /// instead.
    FieldNotNumeric {
        field: &'static str,
        found: &'static str,
        span: Span,
    },
    /// Two `with`-listed templates both set the same scalar/struct
    /// field (`image`/`expose`/`restart`). Per docs/DESIGN.md: "a
    /// collision between two of these on the same scalar/map field is a
    /// compile error." Never raised against the service's own body,
    /// which always silently wins.
    FieldCollision {
        field: &'static str,
        first_template: String,
        second_template: String,
        first: Span,
        second: Span,
    },
    /// Same rule as [`Self::FieldCollision`], for a map field
    /// (`env`/`volume`/`publish`) — two explicit templates set the same
    /// key (`env`), container path (`volume`), or container port
    /// (`publish`) — each matching that field's own existing [`MapSide`]
    /// uniqueness convention. Boxed since
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
    /// field that has no cross-file meaning — `depends_on`, `dns`,
    /// `env_file`, `router.entrypoints`, `router.path_prefix`, or
    /// `router.middleware`. (`depends_on` names
    /// a same-file sibling service; the others aren't resolved against
    /// anything an `.hll` file declares at all — an entry point lives in
    /// the deployment's own `traefik.yml`, and an `env_file` path lives
    /// on disk next to the compose file.) `devices` isn't among these:
    /// since #167 its entries are plain [`Literal`]s, like `publish`'s
    /// and `env`'s, which were never reference-shaped to begin with, so
    /// there's nothing here to reject. See
    /// [`crate::schema::allows_qualified_reference`] for the single
    /// table this list is drawn from. Rejected rather than silently
    /// accepted or silently dropped. Only `networks` and a named-volume
    /// mount's host side resolve a qualifier, because those two really
    /// are declarations another file can export.
    UnsupportedQualifiedReference {
        field: &'static str,
        alias: String,
        span: Span,
    },
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
    /// A qualified `networks [alias.name]` entry resolved to an imported
    /// `network`, but another `network` with the same bare name is
    /// already in scope — the entry file's own declaration, or one
    /// pulled in through a different alias.
    ///
    /// Codegen re-resolves a service's `networks [...]` entries by bare
    /// name against one flat list of declarations, so two networks
    /// sharing a bare name are indistinguishable to it and the first
    /// silently wins. Before this check, that meant asking for
    /// `ext.proxy` and quietly getting the local `proxy` — wrong
    /// Compose output *and* a missing `traefik.docker.network` label,
    /// with no diagnostic at any stage (#71).
    ///
    /// The lasting fix is to preserve the resolved identity on the
    /// `Literal` so codegen never re-resolves by bare name at all;
    /// this error is the contained stopgap, and stays worth keeping
    /// afterwards as a clarity check — two networks sharing one bare
    /// name in a single document is confusing whether or not the
    /// compiler can tell them apart.
    CollidingImportedNetwork {
        alias: String,
        name: String,
        span: Span,
    },
    /// A qualified named-volume mount (`volume alias.name -> "/path"`)
    /// whose `alias` resolved to a real imported scope, but no `volume`
    /// named `name` exists there. The volume-side twin of
    /// [`Self::UnknownQualifiedNetwork`], raised in exactly the same
    /// place and for exactly the same reason.
    UnknownQualifiedVolume {
        alias: String,
        name: String,
        span: Span,
    },
    /// A qualified named-volume mount resolved to an imported `volume`,
    /// but another `volume` with the same bare name is already in scope
    /// — the entry file's own declaration, or one pulled in through a
    /// different alias.
    ///
    /// The volume-side twin of [`Self::CollidingImportedNetwork`], and
    /// unavoidable for the same reason: an imported volume keeps its own
    /// bare name as its key in the generated `volumes:` section, and
    /// codegen resolves every named-volume mount by bare name against
    /// one flat list of declarations. Two volumes sharing one bare name
    /// would be one Compose key claimed by two different declarations,
    /// with the first silently winning.
    CollidingImportedVolume {
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
            | ComposeError::DuplicateVolumeName { second: span, .. }
            | ComposeError::TemplateCycle { span, .. }
            | ComposeError::TemplateNestingTooDeep { span, .. }
            | ComposeError::UnknownTemplateArgument { span, .. }
            | ComposeError::MissingTemplateArgument { span, .. }
            | ComposeError::DuplicateTemplateArgument { second: span, .. }
            | ComposeError::TemplateArgumentNotScalar { span, .. }
            | ComposeError::ArgumentNotReferenceShaped { span, .. }
            | ComposeError::ArgumentNotNumeric { span, .. }
            | ComposeError::FieldNotNumeric { span, .. }
            | ComposeError::FieldCollision { second: span, .. }
            | ComposeError::UnknownAlias { span, .. }
            | ComposeError::UnsupportedQualifiedReference { span, .. }
            | ComposeError::UnknownQualifiedNetwork { span, .. }
            | ComposeError::CollidingImportedNetwork { span, .. }
            | ComposeError::UnknownQualifiedVolume { span, .. }
            | ComposeError::CollidingImportedVolume { span, .. } => *span,
            ComposeError::MapKeyCollision(details) => details.second,
        }
    }

    /// Renders this error with each location it mentions resolved
    /// against `files` — `path:line:col` instead of a bare `line:col`.
    ///
    /// A composed service's fields can come from any file in the `use`
    /// graph, so the two locations in a collision error routinely live
    /// in *different* files; naming both is the whole point of carrying
    /// a [`FileId`](hl_lexer::FileId) on every [`Span`] (#75). Spans
    /// whose file `files` doesn't know still render bare, which is what
    /// the single-file [`Display`](fmt::Display) impl relies on.
    pub fn display<'a>(&'a self, files: &'a SourceMap) -> impl fmt::Display + 'a {
        DisplayComposeError {
            error: self,
            files: Some(files),
        }
    }

    /// The one implementation behind both [`Self::display`] and the
    /// [`Display`](fmt::Display) impl — every location goes through
    /// [`Span::locate`], so the two renderings can't drift apart.
    fn write(&self, f: &mut fmt::Formatter<'_>, files: Option<&SourceMap>) -> fmt::Result {
        let at = self.span().locate(files);
        match self {
            ComposeError::UnknownTemplate { name, .. } => {
                write!(f, "{at}: unknown template `{name}`")
            }
            ComposeError::DuplicateTemplateName { name, first, .. } => write!(
                f,
                "{at}: duplicate template `{name}` (first declared at {})",
                first.locate(files)
            ),
            ComposeError::DuplicateServiceName { name, first, .. } => write!(
                f,
                "{at}: duplicate service `{name}` (first declared at {})",
                first.locate(files)
            ),
            ComposeError::DuplicateNetworkName { name, first, .. } => write!(
                f,
                "{at}: duplicate network `{name}` (first declared at {})",
                first.locate(files)
            ),
            ComposeError::DuplicateVolumeName { name, first, .. } => write!(
                f,
                "{at}: duplicate volume `{name}` (first declared at {})",
                first.locate(files)
            ),
            ComposeError::TemplateCycle { chain, .. } => write!(
                f,
                "{at}: template composition cycle: {}",
                chain.join(" -> ")
            ),
            ComposeError::TemplateNestingTooDeep { name, limit, .. } => write!(
                f,
                "{at}: `with` nesting deeper than {limit} levels (reached at template `{name}`)"
            ),
            ComposeError::UnknownTemplateArgument {
                template, argument, ..
            } => write!(
                f,
                "{at}: unknown argument `{argument}` for template `{template}`"
            ),
            ComposeError::MissingTemplateArgument {
                template, param, ..
            } => write!(
                f,
                "{at}: missing required argument `{param}` for template `{template}`"
            ),
            ComposeError::DuplicateTemplateArgument {
                template,
                argument,
                first,
                ..
            } => write!(
                f,
                "{at}: duplicate argument `{argument}` for template `{template}` (first set at {})",
                first.locate(files)
            ),
            ComposeError::TemplateArgumentNotScalar {
                template, param, ..
            } => write!(
                f,
                "{at}: argument `{param}` for template `{template}` must be a scalar value (a list/map can't fill a single-value field)"
            ),
            ComposeError::ArgumentNotReferenceShaped {
                template, param, ..
            } => write!(
                f,
                "{at}: argument `{param}` for template `{template}` must be reference-shaped (a bare identifier, a quoted string, or `alias.name`) — found a number"
            ),
            ComposeError::ArgumentNotNumeric {
                template,
                param,
                found,
                ..
            } => write!(
                f,
                "{at}: argument `{param}` for template `{template}` must be a number (found {found})"
            ),
            ComposeError::FieldNotNumeric { field, found, .. } => {
                write!(f, "{at}: `{field}` must be a number (found {found})")
            }
            ComposeError::FieldCollision {
                field,
                first_template,
                second_template,
                first,
                ..
            } => write!(
                f,
                "{at}: field `{field}` set by both template `{first_template}` (at {}) and template `{second_template}`—explicit templates must not conflict",
                first.locate(files)
            ),
            ComposeError::MapKeyCollision(details) => {
                let side_desc = match details.side {
                    MapSide::Key => "key",
                    MapSide::Value => "value",
                };
                write!(
                    f,
                    "{at}: `{}` {side_desc} {:?} set by both template `{}` (at {}) and template `{}`—explicit templates must not conflict",
                    details.field,
                    details.key,
                    details.first_template,
                    details.first.locate(files),
                    details.second_template,
                )
            }
            ComposeError::UnknownAlias { alias, .. } => {
                write!(f, "{at}: unknown alias `{alias}`")
            }
            ComposeError::UnsupportedQualifiedReference { field, alias, .. } => write!(
                f,
                "{at}: `{field}` doesn't support a qualified reference yet (`{alias}.` ...)"
            ),
            ComposeError::UnknownQualifiedNetwork { alias, name, .. } => {
                write!(f, "{at}: no network `{name}` in `{alias}`")
            }
            ComposeError::CollidingImportedNetwork { alias, name, .. } => write!(
                f,
                "{at}: `{alias}.{name}` collides with another network named `{name}` \
                 already in scope — networks are resolved by their bare name, so the \
                 two can't be told apart; rename one of them"
            ),
            ComposeError::UnknownQualifiedVolume { alias, name, .. } => {
                write!(f, "{at}: no volume `{name}` in `{alias}`")
            }
            ComposeError::CollidingImportedVolume { alias, name, .. } => write!(
                f,
                "{at}: `{alias}.{name}` collides with another volume named `{name}` \
                 already in scope — volumes are resolved by their bare name, so the \
                 two can't be told apart; rename one of them"
            ),
        }
    }
}

/// [`ComposeError::display`]'s return type: the error plus the map its
/// spans resolve against.
struct DisplayComposeError<'a> {
    error: &'a ComposeError,
    files: Option<&'a SourceMap>,
}

impl fmt::Display for DisplayComposeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.write(f, self.files)
    }
}

impl fmt::Display for ComposeError {
    /// Renders every location as a bare `line:col`, with no file — the
    /// right thing for the single-file [`compose`] entry point, whose
    /// spans have no file identity to render. A caller that has a
    /// [`SourceMap`] (the linker, and through it the CLI) wants
    /// [`ComposeError::display`] instead.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, None)
    }
}

impl std::error::Error for ComposeError {}

#[cfg(test)]
mod error_display_tests {
    use super::*;
    use hl_lexer::FileId;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
            file: FileId::ANONYMOUS,
        }
    }

    /// A span in a file the map knows renders `path:line:col`, and each
    /// location in a two-location error resolves independently — the
    /// point of #75, since the two templates in a collision routinely
    /// live in different files.
    #[test]
    fn display_with_a_source_map_names_each_location_s_file() {
        let mut files = SourceMap::default();
        let one = files.intern("t1.hll");
        let two = files.intern("t2.hll");
        let at = |line, col, file| Span {
            start: 0,
            end: 0,
            line,
            col,
            file,
        };
        let err = ComposeError::FieldCollision {
            field: "restart.policy",
            first_template: "x".to_string(),
            second_template: "y".to_string(),
            first: at(2, 11, one),
            second: at(2, 11, two),
        };
        assert_eq!(
            err.display(&files).to_string(),
            "t2.hll:2:11: field `restart.policy` set by both template `x` (at t1.hll:2:11) \
             and template `y`—explicit templates must not conflict"
        );
        // Bare `Display` is what the single-file `compose` entry point
        // gets, and is unchanged.
        assert_eq!(
            err.to_string(),
            "2:11: field `restart.policy` set by both template `x` (at 2:11) and template `y`\
             —explicit templates must not conflict"
        );
    }

    /// A span whose file the map doesn't know still renders, just
    /// without a path.
    #[test]
    fn display_with_a_source_map_falls_back_for_anonymous_spans() {
        let mut files = SourceMap::default();
        files.intern("entry.hll");
        let err = ComposeError::UnknownTemplate {
            name: "base".to_string(),
            span: span(3, 2),
        };
        assert_eq!(
            err.display(&files).to_string(),
            "3:2: unknown template `base`"
        );
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
    fn duplicate_volume_name_display() {
        let err = ComposeError::DuplicateVolumeName {
            name: "data".to_string(),
            first: span(2, 1),
            second: span(9, 1),
        };
        assert_eq!(
            err.to_string(),
            "9:1: duplicate volume `data` (first declared at 2:1)"
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
    fn template_nesting_too_deep_display() {
        let err = ComposeError::TemplateNestingTooDeep {
            name: "t0".to_string(),
            limit: 64,
            span: span(1, 10),
        };
        assert_eq!(
            err.to_string(),
            "1:10: `with` nesting deeper than 64 levels (reached at template `t0`)"
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
    fn argument_not_reference_shaped_display() {
        let err = ComposeError::ArgumentNotReferenceShaped {
            template: "t".to_string(),
            param: "net".to_string(),
            span: span(2, 2),
        };
        assert_eq!(
            err.to_string(),
            "2:2: argument `net` for template `t` must be reference-shaped (a bare identifier, a quoted string, or `alias.name`) — found a number"
        );
    }

    #[test]
    fn argument_not_numeric_display() {
        let err = ComposeError::ArgumentNotNumeric {
            template: "t".to_string(),
            param: "port".to_string(),
            found: "a quoted string",
            span: span(2, 2),
        };
        assert_eq!(
            err.to_string(),
            "2:2: argument `port` for template `t` must be a number (found a quoted string)"
        );
    }

    #[test]
    fn field_not_numeric_display() {
        let err = ComposeError::FieldNotNumeric {
            field: "expose.port",
            found: "a quoted string",
            span: span(3, 3),
        };
        assert_eq!(
            err.to_string(),
            "3:3: `expose.port` must be a number (found a quoted string)"
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
            "2:1: field `image` set by both template `a` (at 1:1) and template `b`—explicit templates must not conflict"
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
            "2:1: `env` key \"FOO\" set by both template `a` (at 1:1) and template `b`—explicit templates must not conflict"
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
            "2:1: `volume` value \"/data\" set by both template `a` (at 1:1) and template `b`—explicit templates must not conflict"
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
            field: "router.middleware",
            alias: "traefik".to_string(),
            span: span(1, 3),
        }
        .to_string();
        assert_eq!(
            err,
            "1:3: `router.middleware` doesn't support a qualified reference yet (`traefik.` ...)"
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

    #[test]
    fn colliding_imported_network_display() {
        let err = ComposeError::CollidingImportedNetwork {
            alias: "ext".to_string(),
            name: "proxy".to_string(),
            span: span(7, 13),
        };
        assert_eq!(
            err.to_string(),
            "7:13: `ext.proxy` collides with another network named `proxy` already in \
             scope — networks are resolved by their bare name, so the two can't be told \
             apart; rename one of them"
        );
    }

    /// The two volume-side twins read exactly like their network
    /// counterparts above, so the pair is one family of diagnostic
    /// rather than two.
    #[test]
    fn unknown_qualified_volume_display() {
        let err = ComposeError::UnknownQualifiedVolume {
            alias: "storage".to_string(),
            name: "media".to_string(),
            span: span(2, 2),
        };
        assert_eq!(err.to_string(), "2:2: no volume `media` in `storage`");
    }

    #[test]
    fn colliding_imported_volume_display() {
        let err = ComposeError::CollidingImportedVolume {
            alias: "storage".to_string(),
            name: "media".to_string(),
            span: span(7, 10),
        };
        assert_eq!(
            err.to_string(),
            "7:10: `storage.media` collides with another volume named `media` already in \
             scope — volumes are resolved by their bare name, so the two can't be told \
             apart; rename one of them"
        );
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

    /// Resolves a `with`-list invocation's target template.
    /// `qualifier` is `Some` for `with alias.name`, `None` for a bare
    /// `with name`. Always an error if nothing matches: every call here
    /// corresponds to an explicit, user-written invocation, which is now
    /// the only way a template is ever applied (#260).
    fn resolve_template(
        &self,
        scope: Self::Scope,
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
    ) -> Result<(Self::Scope, &TemplateDecl), ComposeError>;

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

    /// Resolves a *qualified* named-volume mount (`volume alias.name ->
    /// "/path"`). The exact counterpart of
    /// [`Self::resolve_qualified_network`], down to when it's called:
    /// never for a bare/unqualified host, which resolves against the
    /// entry file's own declarations at codegen time.
    fn resolve_qualified_volume(
        &self,
        scope: Self::Scope,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Volume, ComposeError>;
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

    fn resolve_qualified_volume(
        &self,
        _scope: (),
        qualifier: &Ident,
        _name: &str,
        _span: Span,
    ) -> Result<&Volume, ComposeError> {
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
/// merged per docs/DESIGN.md's 2-tier priority: each `with`-listed
/// template left-to-right at the lower tier (collisions between two of
/// these are errors), and the service's own body on top (always wins,
/// unconditionally). Resolution stops at the first error, matching
/// [`crate::parse`]'s own no-error-recovery precedent.
pub fn compose(program: Program) -> Result<ComposedProgram, ComposeError> {
    let mut networks = Vec::new();
    let mut volumes = Vec::new();
    let mut services = Vec::new();
    let mut templates: HashMap<String, TemplateDecl> = HashMap::new();
    // Networks, volumes and services are kept as ordered `Vec`s (source
    // order is load-bearing downstream), so unlike templates they need
    // their own by-name tables purely to detect a redeclaration.
    let mut network_spans: HashMap<String, Span> = HashMap::new();
    let mut volume_spans: HashMap<String, Span> = HashMap::new();
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
            TopDecl::Volume(v) => {
                if let Some(&first) = volume_spans.get(&v.name.name) {
                    return Err(ComposeError::DuplicateVolumeName {
                        name: v.name.name.clone(),
                        first,
                        second: v.name.span,
                    });
                }
                volume_spans.insert(v.name.name.clone(), v.name.span);
                volumes.push(v);
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
    compose_with_resolver(networks, volumes, services, (), &resolver)
}

/// The generalized merge engine: composes `services` (plus whatever
/// `networks` their qualified `networks [...]` entries additionally
/// resolve to) by resolving every name through `resolver`, starting from
/// `entry_scope`. See [`SymbolResolver`]'s doc for the scoping contract
/// this implements.
///
/// `volumes` grows the same way `networks` does, and for the same
/// reason: a named-volume mount's host side is a reference-shaped
/// [`Literal`] (`volume alias.name -> "/config"`), so an imported
/// `volume` declaration has to
/// be pulled into the program the mount belongs to before codegen can
/// resolve it there.
pub fn compose_with_resolver<R: SymbolResolver>(
    networks: Vec<Network>,
    volumes: Vec<Volume>,
    services: Vec<Service>,
    entry_scope: R::Scope,
    resolver: &R,
) -> Result<ComposedProgram, ComposeError> {
    let mut cache: HashMap<(R::Scope, String), ServiceFields> = HashMap::new();
    let mut imports = Imports::default();
    let mut composed = Vec::with_capacity(services.len());
    for service in services {
        composed.push(compose_service(
            service,
            entry_scope,
            resolver,
            &mut cache,
            &mut imports,
        )?);
    }

    let mut all_networks = networks;
    merge_imported(&mut all_networks, imports.networks)?;
    let mut all_volumes = volumes;
    merge_imported(&mut all_volumes, imports.volumes)?;

    Ok(ComposedProgram {
        networks: all_networks,
        volumes: all_volumes,
        services: composed,
        // Composition never reads a file, so it has no paths to intern;
        // the linker attaches its own map to what this returns.
        files: SourceMap::default(),
    })
}

/// A declaration an entry-file service reached through a qualified
/// reference (`networks [alias.name]`, `volume alias.name -> "/path"`),
/// kept together with the reference that pulled it in. The reference's
/// own span is what the colliding-import errors point at: the imported
/// declaration lives in another file, and the reference is the line the
/// user would edit to resolve the collision. (Both spans now know which
/// file they belong to — see [`hl_lexer::FileId`] — so pointing at the
/// declaration instead would be renderable; it just isn't the more
/// useful location.)
struct Imported<D> {
    decl: D,
    alias: String,
    reference: Span,
}

/// Everything a service's qualified references dragged in from other
/// files, accumulated across a whole program's composition and folded
/// into the finished [`ComposedProgram`]'s own declaration lists by
/// [`merge_imported`].
#[derive(Default)]
struct Imports {
    networks: Vec<Imported<Network>>,
    volumes: Vec<Imported<Volume>>,
}

/// A top-level declaration a qualified reference can import: one that
/// codegen later re-resolves *by bare name* against one flat list, which
/// is exactly what makes two same-named imports a problem worth naming.
trait ImportableDecl: Clone + PartialEq {
    /// What this declaration is called, and what a Compose section keys
    /// it under.
    fn decl_name(&self) -> &str;
    /// The error to raise when a different declaration already holds
    /// this bare name.
    fn collision(alias: String, name: String, span: Span) -> ComposeError;
}

impl ImportableDecl for Network {
    fn decl_name(&self) -> &str {
        &self.name.name
    }

    fn collision(alias: String, name: String, span: Span) -> ComposeError {
        ComposeError::CollidingImportedNetwork { alias, name, span }
    }
}

impl ImportableDecl for Volume {
    fn decl_name(&self) -> &str {
        &self.name.name
    }

    fn collision(alias: String, name: String, span: Span) -> ComposeError {
        ComposeError::CollidingImportedVolume { alias, name, span }
    }
}

/// Folds every imported declaration into `all`, rejecting a bare-name
/// collision with one already there.
fn merge_imported<D: ImportableDecl>(
    all: &mut Vec<D>,
    imported: Vec<Imported<D>>,
) -> Result<(), ComposeError> {
    for entry in imported {
        match all.iter().find(|d| d.decl_name() == entry.decl.decl_name()) {
            // Already pulled in: the same imported declaration reached
            // here more than once, because more than one service (or
            // more than one reference) named it. One declaration named
            // once, not a collision.
            Some(already) if *already == entry.decl => {}
            // A *different* declaration is already in scope under this
            // bare name. Codegen resolves references by bare name
            // against this one flat list, so the two are
            // indistinguishable there and the first — the entry file's
            // own, since its declarations come first — silently wins
            // (#71). For a network that produced Compose output the user
            // never asked for, plus a missing `traefik.docker.network`
            // label, with no diagnostic anywhere; for a volume it would
            // mount a different underlying volume than the one named.
            // Reject it instead.
            Some(_) => {
                let name = entry.decl.decl_name().to_string();
                return Err(D::collision(entry.alias, name, entry.reference));
            }
            None => all.push(entry.decl),
        }
    }
    Ok(())
}

fn compose_service<R: SymbolResolver>(
    service: Service,
    scope: R::Scope,
    resolver: &R,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    imports: &mut Imports,
) -> Result<Service, ComposeError> {
    let mut acc = MergeAcc::default();
    let mut in_progress = Vec::new();

    for inv in &service.fields.with {
        let resolved = resolve_invocation(inv, scope, resolver, cache, &mut in_progress, imports)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }

    let mut own = service.fields;
    own.with.clear();
    resolve_qualified_references(&mut own, scope, resolver, imports)?;
    merge_tier(&mut acc, own, &Tier::Own)?;

    let fields = acc.into_service_fields();
    check_numeric_fields(&fields)?;
    Ok(Service {
        name: service.name,
        fields,
        span: service.span,
    })
}

/// Resolves a *template's own* composition: its own `with`-list merged
/// (explicit-tier, left-to-right) with its own directly-set fields
/// (always winning over its own `with`-list — the same rule as a
/// service's own body
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
    imports: &mut Imports,
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
    // `in_progress` is pushed on entry and popped on the way out, so its
    // length *is* the current nesting depth — the explicit depth counter
    // this recursion needed (#72). The cycle check above bounds
    // *repetition*, not depth: a non-cyclic chain `t0 <- t1 <- ... <- tN`
    // passes it at every level and used to recurse until the stack
    // overflowed, aborting the process instead of returning an error a
    // library embedder could catch.
    //
    // Written against the 1-based level this call occupies rather than
    // as `len() >= MAX`, which is the same test but has an *equivalent
    // mutant*: `in_progress` only ever grows one at a time, so `==` and
    // `>=` trigger at exactly the same call and no test could tell them
    // apart. `level > MAX` moves every comparison mutant one level off
    // the boundary, where the tests at and past the limit catch it.
    let level = in_progress.len() + 1;
    if level > MAX_TEMPLATE_DEPTH {
        return Err(ComposeError::TemplateNestingTooDeep {
            name: name.clone(),
            limit: MAX_TEMPLATE_DEPTH,
            span: decl.name.span,
        });
    }
    in_progress.push((scope, name.clone()));

    let mut acc = MergeAcc::default();
    for inv in &decl.fields.with {
        let resolved = resolve_invocation(inv, scope, resolver, cache, in_progress, imports)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }
    let mut own = decl.fields.clone();
    own.with.clear();
    resolve_qualified_references(&mut own, scope, resolver, imports)?;
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
    imports: &mut Imports,
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
        if !args.contains_key(param.name.name.as_str()) {
            return Err(ComposeError::MissingTemplateArgument {
                template: decl.name.name.clone(),
                param: param.name.name.clone(),
                span: inv.span,
            });
        }
    }

    let mut fields = resolve_template(decl, target_scope, resolver, cache, in_progress, imports)?;
    substitute_params(&mut fields, &args, &decl.name.name)?;
    Ok(fields)
}

/// Resolves every *qualified* reference in `fields` against `scope` — a
/// `networks [alias.name]` entry and a `volume alias.name -> "/path"`
/// mount's host side are the two the language has — rewriting each to an
/// unqualified, resolved bare reference so [`merge_tier`] never needs to
/// know imports exist, and recording the declaration it reached in
/// `imports`. Every other reference-shaped position rejects a qualified
/// entry outright — [`schema::allows_qualified_reference`] is the single
/// table of which positions are which, and
/// [`ComposeError::UnsupportedQualifiedReference`]'s own doc explains why
/// every rejected position has no cross-file meaning to resolve one
/// against. `devices` needs no such check at all: since #167 its entries
/// are plain [`Literal`]s that were never reference-shaped to begin with
/// (see [`crate::schema::DEVICES`]), so there's no qualifier-carrying
/// slot here to visit.
///
/// A [`Literal::Param`] passing through any of these loops untouched is
/// correct, not an oversight: this runs on a scope's own
/// still-unsubstituted body (its `Tier::Own` step, below), before
/// [`substitute_params`] ever sees it, so an entry a template will later
/// bind to a real value has no qualifier yet to resolve or reject either
/// way — [`Literal::qualifier`] answers `None` for a `Param` exactly as
/// it does for a plain `Ident`, which is what lets every loop here stay
/// silent about it.
///
/// Runs exactly once per scope, at the point that scope's own
/// directly-written fields are merged (its `Tier::Own` step in
/// [`compose_service`]/[`resolve_template`]) — by induction, every
/// `ServiceFields` [`merge_tier`] ever sees has already passed through
/// this, transitively, since a `with`-list target's own qualified
/// references were already resolved when *it* was resolved.
fn resolve_qualified_references<R: SymbolResolver>(
    fields: &mut ServiceFields,
    scope: R::Scope,
    resolver: &R,
    imports: &mut Imports,
) -> Result<(), ComposeError> {
    debug_assert!(schema::allows_qualified_reference("networks"));
    for lit in &mut fields.networks {
        let Literal::Qualified(q) = lit else {
            continue;
        };
        let network = resolver.resolve_qualified_network(scope, &q.qualifier, &q.name, q.span)?;
        imports.networks.push(Imported {
            decl: network.clone(),
            alias: q.qualifier.name.clone(),
            reference: q.span,
        });
        *lit = Literal::Ident(network.name.name.clone(), q.span);
    }
    debug_assert!(schema::allows_qualified_reference("volume"));
    for entry in &mut fields.volumes.entries {
        let ArrowMapHost::Named(lit) = &mut entry.host else {
            continue;
        };
        let Literal::Qualified(q) = lit else {
            continue;
        };
        let volume = resolver.resolve_qualified_volume(scope, &q.qualifier, &q.name, q.span)?;
        imports.volumes.push(Imported {
            decl: volume.clone(),
            alias: q.qualifier.name.clone(),
            reference: q.span,
        });
        *lit = Literal::Ident(volume.name.name.clone(), q.span);
    }
    // `depends_on`'s entries carry a `Literal` rather than being one,
    // since #155 gave each one an optional `condition` alongside it —
    // see [`ast::DependsOnEntry`]'s doc — so this maps down to the
    // literals inside rather than passing the list straight through.
    reject_qualified(fields.depends_on.iter().map(|e| &e.reference), "depends_on")?;
    reject_qualified(&fields.dns, "dns")?;
    // An `env_file` path lives on disk next to the compose file, which
    // no `.hll` file declares, so there's nothing for an alias to
    // resolve against — same reasoning as `router.entrypoints` just
    // below.
    reject_qualified(&fields.env_file, "env_file")?;
    // A `router`'s own `entrypoints` list (#184) names an entry point in
    // the deployment's own `traefik.yml`, which no `.hll` file declares,
    // so there is nothing for an alias to resolve against — and codegen
    // reads only `Literal::text`, so an unchecked `traefik.web` would
    // compile to `entrypoints=web` with the qualifier silently gone.
    // `path_prefix` (#196) gets the same check for the first time here:
    // before #196 it couldn't parse a qualifier at all (see
    // [`crate::schema::allows_qualified_reference`]'s doc), so there was
    // nothing yet to reject.
    // `router.middleware` (#221) names a middleware in that same
    // `traefik.yml`, so it rejects a qualifier for exactly the reason
    // `entrypoints` beside it does.
    // A `rule` matcher's arguments (#228) are the same kind of free
    // text `path_prefix` holds — a hostname, a path, a header value —
    // and land in a Traefik rule the same way, so a qualifier there has
    // nothing to resolve against either.
    for router in &fields.routers {
        reject_qualified(&router.entrypoints, "router.entrypoints")?;
        reject_qualified(&router.path_prefix, "router.path_prefix")?;
        reject_qualified(&router.middleware, "router.middleware")?;
        if let Some(rule) = &router.rule {
            reject_qualified(rule.args(), "router.rule")?;
        }
    }
    Ok(())
}

/// Rejects a qualified entry in any reference-shaped position not listed
/// in [`schema::allows_qualified_reference`] — every call site here
/// names one of the `false` rows that table documents, and the
/// `debug_assert` ties the two together so a row can't silently drift
/// out of sync with which positions actually call this.
fn reject_qualified<'a>(
    values: impl IntoIterator<Item = &'a Literal>,
    field: &'static str,
) -> Result<(), ComposeError> {
    debug_assert!(!schema::allows_qualified_reference(field));
    for v in values {
        if let Some(q) = v.qualifier() {
            return Err(ComposeError::UnsupportedQualifiedReference {
                field,
                alias: q.name.clone(),
                span: v.span(),
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
    // `build`'s own literal slots (#224) — both plain free-text paths,
    // so both take an ordinary `substitute_literal`. Missing either
    // would reproduce #168's live bug class: the `Literal::Param`
    // survives to codegen, which writes the parameter's own name into
    // the generated `build:` key and exits 0.
    if let Some(b) = &mut fields.build {
        if let Some(c) = &mut b.context {
            substitute_literal(c, args, template_name)?;
        }
        if let Some(d) = &mut b.dockerfile {
            substitute_literal(d, args, template_name)?;
        }
    }
    if let Some(e) = &mut fields.expose
        && let Some(p) = &mut e.port
    {
        substitute_numeric_literal(p, args, template_name)?;
    }
    // `router`'s own literal slots (#184, #221) — `host`, `entrypoints`,
    // `path_prefix`, and `middleware` are all `Literal`-carrying (see
    // `schema::FieldKind::ReferenceList`). Missing any of the four would
    // reproduce #168's live bug class: the `Literal::Param` survives to
    // codegen, which emits the parameter's own name into a Traefik rule
    // and exits 0.
    for router in &mut fields.routers {
        if let Some(h) = &mut router.host {
            substitute_literal(h, args, template_name)?;
        }
        for entry in &mut router.entrypoints {
            substitute_reference_literal(entry, args, template_name)?;
        }
        for prefix in &mut router.path_prefix {
            substitute_reference_literal(prefix, args, template_name)?;
        }
        for mw in &mut router.middleware {
            substitute_reference_literal(mw, args, template_name)?;
        }
        // #225: `priority`/`port` are numbers, so they take the same
        // numeric-checked substitution `expose.port` does; `protocol`
        // is a plain literal, validated in codegen rather than here so
        // an unresolved `$proto` never reaches that check.
        if let Some(p) = &mut router.priority {
            substitute_numeric_literal(p, args, template_name)?;
        }
        if let Some(p) = &mut router.port {
            substitute_numeric_literal(p, args, template_name)?;
        }
        if let Some(p) = &mut router.protocol {
            substitute_literal(p, args, template_name)?;
        }
        // #228: every matcher argument in a `rule`, reached through the
        // one walk `reject_qualified` above uses, so the two can't
        // disagree about which slots a rule has. Reference-shaped for
        // the reason `path_prefix` beside it is — a matcher argument is
        // free text, never a number.
        if let Some(rule) = &mut router.rule {
            for arg in rule.args_mut() {
                substitute_reference_literal(arg, args, template_name)?;
            }
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
    // `command`'s literals (#156) go through the same substitution walk
    // as every other `Literal` slot above, so a `$param` reference
    // inside a `command ["--user=$user"]` entry gets resolved here — see
    // `ast::Literal::Param`'s own doc for why a `Param` surviving this
    // pass unresolved would be a bug.
    match &mut fields.command {
        Some(Command::Shell(lit)) => substitute_literal(lit, args, template_name)?,
        Some(Command::Exec(items, _)) => {
            for item in items {
                substitute_literal(item, args, template_name)?;
            }
        }
        None => {}
    }
    // `entrypoint`'s literals (#183) walk exactly like `command`'s just
    // above, and for the same reason: it's the same shell/exec pair of
    // shapes, so the exec form's items each get substituted
    // individually. A `$param` left behind here would reach codegen as
    // the parameter's own name — issue #168's bug class, which is why
    // every new literal-carrying field gets an arm in this walk.
    match &mut fields.entrypoint {
        Some(Entrypoint::Shell(lit)) => substitute_literal(lit, args, template_name)?,
        Some(Entrypoint::Exec(items, _)) => {
            for item in items {
                substitute_literal(item, args, template_name)?;
            }
        }
        None => {}
    }
    // `healthcheck`'s literal-valued sub-fields (#153) walk exactly like
    // `command`'s just above (#168): every one of them is a plain
    // `Literal` slot a `$param` can be written into, and `test` carries
    // the same shell/exec split `command` does, so the exec form's items
    // each get substituted individually. Missing any of them left the
    // `Literal::Param` in place for codegen to emit as the parameter's
    // own name.
    if let Some(hc) = &mut fields.healthcheck {
        match &mut hc.test {
            Some(HealthcheckTest::Shell(lit)) => substitute_literal(lit, args, template_name)?,
            Some(HealthcheckTest::Exec(items, _)) => {
                for item in items {
                    substitute_literal(item, args, template_name)?;
                }
            }
            None => {}
        }
        // `retries` is `book/src/built-in-fields.md`'s other `number`-typed
        // field alongside `expose.port`, so it takes the numeric-checked
        // substitution rather than riding the loop below with its four
        // string-typed siblings.
        if let Some(retries) = &mut hc.retries {
            substitute_numeric_literal(retries, args, template_name)?;
        }
        for lit in [
            hc.interval.as_mut(),
            hc.timeout.as_mut(),
            hc.start_period.as_mut(),
            hc.start_interval.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            substitute_literal(lit, args, template_name)?;
        }
    }
    // `volume`, `publish`, and `devices` share one entry type since
    // #192, so they share one walk rather than three near-identical
    // ones. Only a bind-mount host holds a literal to substitute into: a
    // named-volume host is only ever [`Literal::Ident`]/
    // [`Literal::Qualified`], never [`Literal::Param`] — the parser
    // routes a `$` token to the `BindMount` arm below instead (see
    // [`crate::parser`]'s own `parse_mount_map_entry`), since a named
    // volume's *identity* isn't the kind of thing #196 set out to make
    // parameterizable, only the free-text positions were. Skipping that
    // arm is therefore correct for `volume` and unreachable for the
    // other two, whose schemas never set `key_may_be_reference` — and
    // `debug_assert` says so out loud rather than leaving a `$param` to
    // survive composition unresolved if that ever changes (#168's bug
    // class, which nothing downstream would catch: codegen raises
    // `UnsubstitutedParameter` for `raw` values only).
    for (field_name, entries) in [
        ("volume", &mut fields.volumes.entries),
        ("publish", &mut fields.publish.entries),
        ("devices", &mut fields.devices.entries),
    ] {
        for entry in entries.iter_mut() {
            match &mut entry.host {
                ArrowMapHost::BindMount(host) => {
                    substitute_literal(host, args, template_name)?;
                }
                ArrowMapHost::Named(_) => debug_assert_eq!(
                    field_name, "volume",
                    "only `volume` sets key_may_be_reference, so only its \
                     entries can carry a named host"
                ),
            }
            substitute_literal(&mut entry.container, args, template_name)?;
        }
    }
    for e in &mut fields.env.entries {
        substitute_literal(&mut e.key, args, template_name)?;
        substitute_literal(&mut e.value, args, template_name)?;
    }
    // `labels` (#243) substitutes exactly like `env` just above: both
    // sides are plain `Literal` slots, so a template may parameterize
    // either a label's key or its value.
    for e in &mut fields.labels.entries {
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
    // The reference-shaped list fields #196 newly opened to `$param` —
    // `networks`, `dns`, `env_file`, and a `depends_on`
    // entry's own reference (`router.entrypoints`, `router.path_prefix`,
    // and `router.middleware` are the same kind of field but already got
    // substituted in the router loop above) — walk through
    // `substitute_reference_literal`
    // rather than plain `substitute_literal`: #201 dropped
    // `: Number`/`: String` parameter annotations, so this is the one
    // place left that still rejects a substituted bare number, since
    // these positions' own grammar could never hold one directly even
    // written by hand. Before #196 none of
    // these could hold a `Literal::Param` at all (they were
    // `Reference`-typed, and a `Reference` had nowhere to put one), so
    // this walk simply didn't exist; missing any one of these rows now
    // would reproduce #168's bug class in a new position — a `$net` that
    // survives composition unresolved and reaches codegen as the literal
    // text `net`.
    for lit in &mut fields.networks {
        substitute_reference_literal(lit, args, template_name)?;
    }
    for lit in &mut fields.dns {
        substitute_reference_literal(lit, args, template_name)?;
    }
    for lit in &mut fields.env_file {
        substitute_reference_literal(lit, args, template_name)?;
    }
    for entry in &mut fields.depends_on {
        substitute_reference_literal(&mut entry.reference, args, template_name)?;
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

/// [`substitute_literal`], plus a check that the substituted argument is
/// actually reference-shaped — the substitution-time replacement #201
/// gave `substitute_params`' reference-list rows (`networks`,
/// `networks`, `dns`, `env_file`, a `depends_on` entry's own reference,
/// `expose.entrypoint`, `router.entrypoints`, `router.path_prefix`) once
/// `: Number`/`: String` annotations stopped existing to check at the
/// call site.
///
/// The check itself: a reference-shaped position's own grammar
/// (`parser::Parser::parse_literal_reference`) can never parse a bare
/// number directly, only `IDENT`, `STRING`, `alias.name`, or `$param` —
/// so if `substitute_literal` leaves a [`Literal::Number`] sitting in one
/// of these slots, the only way it could have gotten there is a template
/// caller passing a bare number as the argument. `param_name` is
/// captured *before* calling `substitute_literal`, since a successful
/// substitution overwrites `lit` (and therefore loses `Literal::Param`'s
/// own name) with the caller's literal.
///
/// That overwrite is also why [`ComposeError::ArgumentNotReferenceShaped`]'s
/// span still names the offending argument rather than the `$param`
/// reference inside the template body: substitution replaces the whole
/// `Literal`, span included, so `lit.span()` after the call is always the
/// caller's own span, not the use site's — the same span
/// `resolve_invocation`'s old call-site check
/// used, before #201 moved the check here.
///
/// A slot that was never a `Literal::Param` to begin with (an ordinary
/// `networks [foo]` entry, written directly) needs no check at all:
/// `parse_literal_reference` already guarantees it can't be a number.
fn substitute_reference_literal(
    lit: &mut Literal,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    substitute_literal(lit, args, template_name)?;
    if let (Some(param), Literal::Number { span, .. }) = (param_name, &*lit) {
        return Err(ComposeError::ArgumentNotReferenceShaped {
            template: template_name.to_string(),
            param,
            span: *span,
        });
    }
    Ok(())
}

/// The `found` text for a numeric-field mismatch — `None` if `lit` is
/// already the [`Literal::Number`] `expose.port`/`healthcheck.retries`
/// need, per `book/src/built-in-fields.md`'s own `number`-typed rows for
/// each. Shared by [`substitute_numeric_literal`] (the substituted-
/// argument path) and [`check_numeric_fields`] (the hand-written
/// backstop) so both diagnostics describe the same mismatch the same
/// way.
///
/// `Literal::Param` also answers `None` — not because it's numeric, but
/// because it isn't resolved *yet*: a template forwarding its own
/// parameter into a nested `with` invocation (`template outer(x) { with
/// inner { y: $x } }`) leaves `substitute_literal` replacing one
/// `Literal::Param` with another here, since `outer`'s own `$x` isn't
/// bound to a concrete value until `outer` itself is invoked. Checking
/// now would either false-positive on a perfectly good forwarded number
/// or miss a genuinely bad one, since this literal's real kind isn't
/// decided yet; the eventual concrete substitution, at whichever call
/// site finally binds `x`, is what this function runs against instead.
fn numeric_mismatch(lit: &Literal) -> Option<&'static str> {
    match lit {
        Literal::Number { .. } | Literal::Param(_, _) => None,
        Literal::Str(_, _) => Some("a quoted string"),
        Literal::Ident(_, _) => Some("a bare identifier"),
        Literal::Qualified(_) => Some("a qualified reference"),
    }
}

/// [`substitute_literal`], plus a check that the substituted argument is
/// a bare number — the companion to [`substitute_reference_literal`] for
/// `expose.port`/`healthcheck.retries`, the two positions
/// `book/src/built-in-fields.md` documents as `number`-typed. Dropping
/// `: Number`/`: String` annotations (#201) took away these fields'
/// declaration-site check exactly as it did the reference-shaped ones,
/// so they get the same substitution-time replacement, for the same
/// reason and via the same span trick: substitution overwrites the whole
/// `Literal`, span included, so the span this leaves behind on a
/// mismatch is always the caller's own argument, not the `$param` use
/// site.
///
/// [`ComposeError::FieldNotNumeric`] is the backstop for the mismatch
/// this can't see: a non-numeric `expose.port`/`healthcheck.retries`
/// written directly, with no `$param` — and therefore no
/// `Literal::Param` for this function to ever be called on in the first
/// place, since [`substitute_params`] only routes a slot through here
/// when substitution actually finds one.
fn substitute_numeric_literal(
    lit: &mut Literal,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    substitute_literal(lit, args, template_name)?;
    let Some(param) = param_name else {
        return Ok(());
    };
    if let Some(found) = numeric_mismatch(lit) {
        return Err(ComposeError::ArgumentNotNumeric {
            template: template_name.to_string(),
            param,
            found,
            span: lit.span(),
        });
    }
    Ok(())
}

/// The backstop [`substitute_numeric_literal`]'s own doc points to: a
/// non-numeric `expose.port`/`healthcheck.retries` that never passed
/// through a `$param` at all, whether written directly by a plain
/// service or inside a template's own body. Runs once per finished
/// service, on its fully merged [`ServiceFields`] — after every tier has
/// merged and every `$param` in scope has resolved — so it sees exactly
/// the literal codegen would have, and names the field rather than a
/// template/parameter pair, since by this point there's no longer one to
/// name: `service`/`healthcheck` don't record which tier a merged
/// field's final value came from.
fn check_numeric_fields(fields: &ServiceFields) -> Result<(), ComposeError> {
    if let Some(port) = fields.expose.as_ref().and_then(|e| e.port.as_ref())
        && let Some(found) = numeric_mismatch(port)
    {
        return Err(ComposeError::FieldNotNumeric {
            field: "expose.port",
            found,
            span: port.span(),
        });
    }
    if let Some(retries) = fields.healthcheck.as_ref().and_then(|h| h.retries.as_ref())
        && let Some(found) = numeric_mismatch(retries)
    {
        return Err(ComposeError::FieldNotNumeric {
            field: "healthcheck.retries",
            found,
            span: retries.span(),
        });
    }
    // A router's own `priority`/`port` (#225) — both numbers, both
    // checked here for the hand-written case exactly as `expose.port`
    // is, since a mismatch written directly never passes through
    // substitution for `substitute_numeric_literal` to catch.
    for router in &fields.routers {
        for (field, slot) in [
            ("router.priority", router.priority.as_ref()),
            ("router.port", router.port.as_ref()),
        ] {
            if let Some(lit) = slot
                && let Some(found) = numeric_mismatch(lit)
            {
                return Err(ComposeError::FieldNotNumeric {
                    field,
                    found,
                    span: lit.span(),
                });
            }
        }
    }
    Ok(())
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
// `networks` entry or named-volume host has already been rewritten to a
// plain resolved reference by `resolve_qualified_references`, and a qualified
// `networks`/`depends_on` entry can never reach here at all, having
// already been rejected). Kept byte-for-byte the same as before imports
// existed, deliberately, since it's the single largest, most-tested
// piece of this module.

/// Which priority tier a value came from, per docs/DESIGN.md's Composition
/// section: `Explicit(template_name)` (left-to-right among themselves) <
/// `Own` (the service's/template's own body).
///
/// #260 removed a third, lowest tier, `Defaults`, when the implicit
/// `defaults` template went: it was the one tier that never
/// participated in conflict checking, so every merge below had to spell
/// out an "always silently loses" arm beside the real rule.
#[derive(Debug, Clone, PartialEq)]
enum Tier {
    Explicit(String),
    Own,
}

trait Spanned {
    fn span(&self) -> Span;
}
/// Shared by `volume`, `publish`, and `devices` (#192) — one impl where
/// there used to be three, one per now-merged entry type.
impl Spanned for ArrowMapEntry {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for EnvEntry {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for LabelEntry {
    fn span(&self) -> Span {
        self.span
    }
}
impl Spanned for RawEntry {
    fn span(&self) -> Span {
        self.span
    }
}
/// The accumulator a field-bag's tiers merge into, tracking which tier
/// last set each value so [`merge_scalar`]/[`merge_map`] can tell
/// "explicit-vs-explicit" (an error) apart from "anything-vs-own"
/// or "anything-vs-own" (silent overrides).
///
/// `scalars` holds every single-value collision point in the language —
/// `image.ref`, `expose.port`, `restart.policy`,
/// `container_name`, `healthcheck`'s five plain-`Literal` sub-fields,
/// and any future one — keyed
/// generically by name, always the fully-dotted canonical path down to
/// the concrete sub-field (`image`/`expose`/`restart` each have one
/// today), never a struct's own bare name, so a field's
/// key is a stable function of its own identity rather than how many
/// siblings its struct happens to have today (see #27: keying a
/// single-field struct under its bare name meant the key would have to
/// change out from under `image.ref`/`restart.policy` the moment either
/// struct grew a second field). A bare field with no enclosing struct at
/// all, like `container_name`, is keyed under its own name — there's no
/// sub-field path to be dotted onto.
///
/// The value each key maps to is a [`ScalarValue`], not a bare
/// [`Literal`]: most rows are `ScalarValue::Literal`, but
/// `healthcheck.test`/`command`/`entrypoint` (whose own AST types,
/// [`HealthcheckTest`]/[`Command`]/[`Entrypoint`], carry Compose's
/// shell-string-or-exec-list shape rather than a plain literal) and
/// `healthcheck.disable`/`traefik.disable`/`privileged` (bare-presence
/// [`FieldKind::BoolFlag`]s, whose only "value" is the span they were
/// set at — see [`crate::schema::FieldKind::BoolFlag`]) ride the same
/// map by going through `ScalarValue`'s other two arms instead (#197).
/// Which canonical keys exist, and how each one's value is read out of/
/// written back into `ServiceFields`, is entirely described by the
/// [`SCALAR_FIELDS`] table below — see its doc, and [`ScalarValue`]'s
/// own, for why that's what makes a new scalar-or-scalar-like collision
/// point a one-line addition rather than a new `MergeAcc` field plus new
/// hand-written merge/rebuild logic.
///
/// `lists` is the same idea for every plain reference-list field —
/// `networks`/`dns`/`env_file`, the three bare ones directly
/// on `ServiceFields`. They carry no `Tier`: list fields concatenate
/// unconditionally, so there is no collision to attribute to a tier.
/// See [`LIST_FIELDS`].
/// `router.entrypoints`/`router.path_prefix`/`router.middleware` aren't
/// among them, even though they're the same [`FieldKind::ReferenceList`]
/// kind — they live one level deeper, under a router name, so they merge
/// through [`Self::routers`] instead (see [`merge_routers`]'s own doc).
/// `devices` isn't among them either — see [`Self::arrow_maps`]'s own doc
/// for why it moved onto the same `merge_map` path as
/// `env`/`volume`/`publish` (#167).
///
/// `depends_on` isn't one of the four — it moved into its own
/// `depends_on` field below, merged key-by-key on the referenced
/// service's own name through [`merge_depends_on`] rather than through
/// [`LIST_FIELDS`], once #155 gave each entry an optional `condition`
/// that two entries naming the same service could actually disagree
/// about. Own always wins over whatever a template said about the same
/// dependency, exactly like every other
/// keyed field — but unlike `env`/`volume`/`publish`'s own
/// [`merge_map`], two `with`-listed templates naming the same service
/// only collide when their *effective* conditions actually differ.
/// Two templates both writing a plain `depends_on [db]` — by far the
/// common case, and the only shape this field had before #155 — still
/// silently collapse to one entry exactly as they always have: see
/// [`merge_depends_on`]'s own doc for why treating that as a collision
/// would be a gratuitous, unmotivated break of every `.hll` file
/// already composing two templates that each depend on the same
/// service.
#[derive(Default)]
struct MergeAcc {
    scalars: HashMap<&'static str, (ScalarValue, Tier)>,
    lists: HashMap<&'static str, Vec<Literal>>,
    /// `volume`/`publish`/`devices`'s shared merge point (#192): one
    /// [`crate::ast::ArrowMapEntry`] bucket per field, keyed the same way
    /// [`Self::scalars`]/[`Self::lists`] are — by the field's own schema
    /// name — rather than three separate `Vec` fields, now that all three
    /// merge through the same [`merge_map`] on the same
    /// [`crate::schema::MapSide::Value`] convention (see
    /// [`ARROW_MAP_FIELDS`]). `devices` joined this path at #167, moved
    /// here from [`Self::lists`] once its entries stopped being plain
    /// [`Reference`]s and gained the same `host -> container` shape
    /// `publish`'s entries already had; #192 then folded its bucket
    /// together with `volume`'s and `publish`'s own, since by that point
    /// all three were already identical `merge_map` calls differing only
    /// in which field name and which `ServiceFields` slot they read.
    /// `env` stays its own [`Self::env`] field below rather than joining
    /// this map: it keys on [`crate::schema::MapSide::Key`] instead of
    /// `Value`, and its entries are [`EnvEntry`], not
    /// [`crate::ast::ArrowMapEntry`], so it shares `merge_map` itself but
    /// not this table-driven grouping.
    arrow_maps: HashMap<&'static str, Vec<(ArrowMapEntry, Tier)>>,
    env: Vec<(EnvEntry, Tier)>,
    /// `labels`' own merge point (#243) — its own field beside
    /// [`Self::env`] for exactly the reason `env` has one: both key on
    /// [`crate::schema::MapSide::Key`] rather than `Value`, and both
    /// carry their own entry type rather than [`ArrowMapEntry`], so
    /// neither fits [`Self::arrow_maps`]' table-driven grouping. Merged
    /// through the same [`merge_map`] `env` goes through, with the same
    /// tier rules: own wins over any template,
    /// and two explicit `with`-listed templates setting one label key
    /// collide with [`ComposeError::MapKeyCollision`] rather than the
    /// second silently overwriting the first.
    labels: Vec<(LabelEntry, Tier)>,
    /// `depends_on`'s own merge point — see this struct's own doc for
    /// why it's merged like a map field (keyed by the referenced
    /// service's name) rather than riding [`Self::lists`].
    depends_on: Vec<(DependsOnEntry, Tier)>,
    /// `raw`'s own merge point (#193) — moved here from a bare [`RawMap`]
    /// once `raw` stopped being the language's one unconditionally
    /// concatenated map field. Merged key-by-key through the same
    /// [`merge_map`] `env` uses, keyed the same way
    /// ([`MapSide::Key`]), now that [`crate::schema::RAW`]'s own `uniqueness`
    /// names one: own always wins, and two
    /// explicit `with`-listed templates setting the same key collide
    /// with [`ComposeError::MapKeyCollision`] instead of the second one
    /// silently overwriting the first.
    raw: Vec<(RawEntry, Tier)>,
    /// `router`'s own merge point (#184), keyed by router name and
    /// merged *per sub-field* within each key — see [`merge_routers`].
    ///
    /// Not [`merge_map`], despite being keyed: `merge_map` replaces a
    /// colliding entry outright, which would mean a service body writing
    /// `router api { host: "..." }` silently discarded the `entrypoints`
    /// and `path_prefix` a template gave the same router. `expose`
    /// already merges per sub-field for exactly this reason, and a
    /// `router` is `expose`'s router half made repeatable, so it has to
    /// keep that property — one level deeper, since the sub-fields sit
    /// under a name rather than directly on the struct.
    routers: Vec<RouterAcc>,
}

/// One router's own accumulator, keyed by [`Self::key`] — a [`Router`]
/// mid-merge, with the tier that last set the scalar `host` tracked
/// alongside it exactly as [`MergeAcc::scalars`] tracks its own.
///
/// `entrypoints`, `path_prefix`, and `middleware` carry no `Tier`: like
/// every other list in the language they concatenate unconditionally, so
/// there's no collision to attribute to a tier. The scalars all do, for
/// the reason [`MergeAcc::scalars`] tracks its own.
struct RouterAcc {
    name: Option<Ident>,
    host: Option<(Literal, Tier)>,
    entrypoints: Vec<Literal>,
    path_prefix: Vec<Literal>,
    middleware: Vec<Literal>,
    /// #225's three scalars, each tier-tracked exactly like `host` and
    /// merged through the same [`merge_router_scalar`].
    priority: Option<(Literal, Tier)>,
    port: Option<(Literal, Tier)>,
    protocol: Option<(Literal, Tier)>,
    /// #228's whole-rule spelling. Tier-tracked like the scalars beside
    /// it and merged through the same [`merge_router_scalar`]: a rule is
    /// one whole record, so two of them are two answers to one question
    /// rather than two halves of one, and concatenating them the way the
    /// lists above concatenate would mean nothing.
    rule: Option<(MatchExpr, Tier)>,
    span: Span,
}

impl RouterAcc {
    fn key(&self) -> Option<&str> {
        self.name.as_ref().map(|n| n.name.as_str())
    }
}

/// A router id as a diagnostic renders it. The unnamed `router { }` form
/// has a real id — the service's own name — but nothing the *user* wrote
/// to quote back, so it's named for what it is rather than as an empty
/// string.
fn router_key_display(key: Option<&str>) -> String {
    key.unwrap_or("<unnamed>").to_string()
}

impl MergeAcc {
    fn into_service_fields(mut self) -> ServiceFields {
        let mut fields = ServiceFields {
            env: EnvMap {
                entries: self.env.into_iter().map(|(v, _)| v).collect(),
            },
            labels: LabelMap {
                entries: self.labels.into_iter().map(|(v, _)| v).collect(),
            },
            depends_on: self.depends_on.into_iter().map(|(v, _)| v).collect(),
            raw: RawMap {
                entries: self.raw.into_iter().map(|(v, _)| v).collect(),
            },
            ..Default::default()
        };
        // `volume`/`publish`/`devices` (#192) — see [`Self::arrow_maps`]'s
        // own doc. A field this loop never touches (nothing set it in any
        // tier) simply keeps the empty `ArrowMap` `Default::default()`
        // already gave it above.
        for field in ARROW_MAP_FIELDS {
            if let Some(entries) = self.arrow_maps.remove(field.key) {
                (field.set)(
                    &mut fields,
                    ArrowMap {
                        entries: entries.into_iter().map(|(v, _)| v).collect(),
                    },
                );
            }
        }
        // Order within this loop is span-preference order, not just table
        // order — see [`SCALAR_FIELDS`]'s own doc. `healthcheck.test` and
        // `.disable` sort after the rest of `healthcheck`'s sub-fields (and
        // `.disable` after `.test`) so a `get_or_insert` that has to
        // materialize `Healthcheck` from scratch always stamps its span
        // from the most specific sub-field present, exactly as before this
        // table absorbed the two rows (#197).
        for field in SCALAR_FIELDS {
            if let Some((value, _)) = self.scalars.remove(field.key) {
                (field.set)(&mut fields, value);
            }
        }
        // Order relative to `SCALAR_FIELDS` above no longer matters for
        // span preference the way it once did for `expose.entrypoint`:
        // every row left in `LIST_FIELDS` (`networks`/`dns`/
        // `env_file`) sits directly on `ServiceFields`, with no nested
        // struct for a `set` to `get_or_insert` and no span of its own to
        // race against.
        for field in LIST_FIELDS {
            if let Some(values) = self.lists.remove(field.key) {
                (field.set)(&mut fields, values);
            }
        }
        // In accumulated order, which is tier order: each `with`
        // target's left to right, then the body's own, with a name an
        // earlier tier already contributed merged in place rather than
        // appended. That's what makes label
        // emission order a stable function of the source (#184).
        fields.routers = self
            .routers
            .into_iter()
            .map(|r| Router {
                name: r.name,
                host: r.host.map(|(lit, _)| lit),
                entrypoints: r.entrypoints,
                path_prefix: r.path_prefix,
                middleware: r.middleware,
                priority: r.priority.map(|(lit, _)| lit),
                port: r.port.map(|(lit, _)| lit),
                protocol: r.protocol.map(|(lit, _)| lit),
                rule: r.rule.map(|(expr, _)| expr),
                span: r.span,
            })
            .collect();
        fields
    }
}

/// The value one [`SCALAR_FIELDS`] row carries (#197). Most rows are
/// [`Self::Literal`] — a plain scalar collision point, same as before this
/// type existed. The other two arms generalize the table over the two
/// shapes a scalar-*like* collision point can take, so a field whose slot
/// isn't a bare [`Literal`] can still ride this one table instead of a
/// bespoke `MergeAcc` field:
///
/// - [`Self::List`] is Compose's own shell-string-or-exec-list shape —
///   the shell form rides [`Self::Literal`] instead, so this arm only
///   ever holds the *exec* form's item list plus its brackets' span.
///   [`HealthcheckTest`], [`Command`], and [`Entrypoint`] each convert to
///   and from this pair of arms in their row's own `take`/`set` — they
///   stay separate AST types (see each one's own doc for why: they're
///   three different Compose keys, and collapsing them would blur that),
///   but they share one merge-time shape, so one pair of arms serves all
///   three.
/// - [`Self::Flag`] is a bare-presence [`crate::schema::FieldKind::BoolFlag`]
///   field's "value": there is nothing to carry but the span it was set
///   at, mirroring how [`Literal::span`] is all [`merge_scalar`] ever
///   needs from a [`Self::Literal`] too.
///
/// [`Self::span`] is what [`merge_scalar`] calls to report a collision,
/// exactly as it once called [`Literal::span`] directly.
#[derive(Debug, Clone, PartialEq)]
enum ScalarValue {
    Literal(Literal),
    List(Vec<Literal>, Span),
    Flag(Span),
}

impl ScalarValue {
    fn span(&self) -> Span {
        match self {
            ScalarValue::Literal(lit) => lit.span(),
            ScalarValue::List(_, span) | ScalarValue::Flag(span) => *span,
        }
    }
}

/// One scalar (or scalar-*like*, see [`ScalarValue`]) collision point in
/// `ServiceFields` — a slot that lives either directly on `ServiceFields`
/// (`container_name`, `command`, `entrypoint`, `privileged`) or inside one
/// of its `Nested` struct fields (`image.ref`, `expose.port`,
/// `restart.policy`, every `healthcheck` sub-field, `traefik.disable`) —
/// described generically by `key` (the identity-stable, fully-dotted name
/// [`merge_scalar`]/`ComposeError` key collisions by — see #27) plus a
/// pair of function pointers for reading the slot out of a tier's
/// `ServiceFields` (`take`) and writing a merged value back into a
/// freshly rebuilt one (`set`). This table is what lets [`merge_tier`]
/// and [`MergeAcc::into_service_fields`] each be one generic loop
/// instead of the two bespoke, hand-enumerated functions they used to
/// be (see hl-lang#28) — the only place left that needs to know
/// `ServiceFields`'s concrete struct shape. Adding a future scalar-or-
/// scalar-like collision point means adding one `ScalarField` entry here,
/// not touching either generic function or `MergeAcc` itself (#197) —
/// `take`/`set` are exactly where a row's own AST type (if it isn't a
/// bare [`Literal`]) converts to and from [`ScalarValue`], so that
/// knowledge stays local to the one row that needs it.
///
/// `expose` is down to its one field, `port`, since #198 moved `host`
/// and `entrypoint` onto `router` — so its `set`'s `get_or_insert`
/// always stamps a freshly created `Expose`'s span from `port` itself,
/// with no sibling sub-field left to race against for span preference
/// the way `healthcheck`'s several still do: `healthcheck.test` sorts
/// after `healthcheck`'s five plain-`Literal` sub-fields, and `.disable`
/// after `.test`, so a `get_or_insert` that has to materialize
/// `Healthcheck` from scratch always stamps its span from the most
/// specific sub-field actually present (#197).
struct ScalarField {
    key: &'static str,
    take: fn(&mut ServiceFields) -> Option<ScalarValue>,
    set: fn(&mut ServiceFields, ScalarValue),
}

/// Unwraps a [`ScalarValue`] a `set` closure knows — by construction, since
/// it's paired one-to-one with a `take` closure that only ever produces
/// this same arm for this same [`ScalarField::key`] — can only be
/// [`ScalarValue::Literal`]. Shared by every plain-`Literal` row below so
/// the panic message names the row that would have to break this
/// invariant, rather than repeating a bespoke `unreachable!()` per row.
fn expect_literal(value: ScalarValue, key: &'static str) -> Literal {
    match value {
        ScalarValue::Literal(lit) => lit,
        ScalarValue::List(..) | ScalarValue::Flag(_) => {
            unreachable!("`{key}`'s own `take` only ever produces `ScalarValue::Literal`")
        }
    }
}

static SCALAR_FIELDS: &[ScalarField] = &[
    ScalarField {
        key: "image.ref",
        take: |f| {
            f.image
                .take()
                .and_then(|i| i.reference)
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "image.ref");
            f.image = Some(Image {
                span: v.span(),
                reference: Some(v),
            });
        },
    },
    // `build`'s two scalars (#224). `context` sorts before `dockerfile`
    // for [`SCALAR_FIELDS`]' span-preference reason: a `get_or_insert`
    // that has to materialize `Build` from scratch stamps its span from
    // the context, the field that names what's being built.
    ScalarField {
        key: "build.context",
        take: |f| {
            f.build
                .as_mut()
                .and_then(|b| b.context.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "build.context");
            let span = v.span();
            f.build.get_or_insert(empty_build(span)).context = Some(v);
        },
    },
    ScalarField {
        key: "build.dockerfile",
        take: |f| {
            f.build
                .as_mut()
                .and_then(|b| b.dockerfile.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "build.dockerfile");
            let span = v.span();
            f.build.get_or_insert(empty_build(span)).dockerfile = Some(v);
        },
    },
    ScalarField {
        key: "expose.port",
        take: |f| {
            f.expose
                .as_mut()
                .and_then(|e| e.port.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "expose.port");
            let span = v.span();
            f.expose.get_or_insert(Expose { port: None, span }).port = Some(v);
        },
    },
    ScalarField {
        key: "restart.policy",
        take: |f| {
            f.restart
                .take()
                .and_then(|r| r.policy)
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "restart.policy");
            f.restart = Some(Restart {
                span: v.span(),
                policy: Some(v),
            });
        },
    },
    ScalarField {
        key: "container_name",
        take: |f| f.container_name.take().map(ScalarValue::Literal),
        set: |f, v| f.container_name = Some(expect_literal(v, "container_name")),
    },
    ScalarField {
        key: "healthcheck.interval",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.interval.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "healthcheck.interval");
            let span = v.span();
            f.healthcheck
                .get_or_insert(empty_healthcheck(span))
                .interval = Some(v);
        },
    },
    ScalarField {
        key: "healthcheck.timeout",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.timeout.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "healthcheck.timeout");
            let span = v.span();
            f.healthcheck.get_or_insert(empty_healthcheck(span)).timeout = Some(v);
        },
    },
    ScalarField {
        key: "healthcheck.retries",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.retries.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "healthcheck.retries");
            let span = v.span();
            f.healthcheck.get_or_insert(empty_healthcheck(span)).retries = Some(v);
        },
    },
    ScalarField {
        key: "healthcheck.start_period",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.start_period.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "healthcheck.start_period");
            let span = v.span();
            f.healthcheck
                .get_or_insert(empty_healthcheck(span))
                .start_period = Some(v);
        },
    },
    ScalarField {
        key: "healthcheck.start_interval",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.start_interval.take())
                .map(ScalarValue::Literal)
        },
        set: |f, v| {
            let v = expect_literal(v, "healthcheck.start_interval");
            let span = v.span();
            f.healthcheck
                .get_or_insert(empty_healthcheck(span))
                .start_interval = Some(v);
        },
    },
    // `healthcheck.test`'s own collision point (#153) — not a plain
    // `Literal`, since [`HealthcheckTest`] carries Compose's own
    // shell-string-or-exec-list shape, so it goes through
    // [`ScalarValue::List`] for the exec form (the shell form still rides
    // [`ScalarValue::Literal`]). Sorted after every plain-`Literal`
    // `healthcheck.*` row above, and before `.disable` below, for the
    // span-preference reasons [`ScalarField`]'s own doc explains.
    ScalarField {
        key: "healthcheck.test",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.test.take())
                .map(|test| match test {
                    HealthcheckTest::Shell(lit) => ScalarValue::Literal(lit),
                    HealthcheckTest::Exec(items, span) => ScalarValue::List(items, span),
                })
        },
        set: |f, v| {
            let test = match v {
                ScalarValue::Literal(lit) => HealthcheckTest::Shell(lit),
                ScalarValue::List(items, span) => HealthcheckTest::Exec(items, span),
                ScalarValue::Flag(_) => {
                    unreachable!(
                        "`healthcheck.test`'s own `take` never produces `ScalarValue::Flag`"
                    )
                }
            };
            let span = test.span();
            f.healthcheck.get_or_insert(empty_healthcheck(span)).test = Some(test);
        },
    },
    // `healthcheck.disable`'s own collision point. A
    // `FieldKind::BoolFlag` carries no value beyond bare presence, so this
    // row's `take`/`set` round-trip through [`ScalarValue::Flag`] instead
    // of `Literal`/`List`.
    ScalarField {
        key: "healthcheck.disable",
        take: |f| {
            f.healthcheck
                .as_mut()
                .and_then(|h| h.disable.take())
                .map(ScalarValue::Flag)
        },
        set: |f, v| {
            let ScalarValue::Flag(span) = v else {
                unreachable!(
                    "`healthcheck.disable`'s own `take` only ever produces `ScalarValue::Flag`"
                )
            };
            f.healthcheck.get_or_insert(empty_healthcheck(span)).disable = Some(span);
        },
    },
    // `traefik.disable`'s own collision point (#159) — same
    // `ScalarValue::Flag` shape as `healthcheck.disable` just above, and
    // for the same reason. Nothing else lives in `Traefik` yet, so there's
    // no sibling sub-field for a freshly materialized one's span to
    // prefer over this one.
    ScalarField {
        key: "traefik.disable",
        take: |f| {
            f.traefik
                .as_mut()
                .and_then(|t| t.disable.take())
                .map(ScalarValue::Flag)
        },
        set: |f, v| {
            let ScalarValue::Flag(span) = v else {
                unreachable!(
                    "`traefik.disable`'s own `take` only ever produces `ScalarValue::Flag`"
                )
            };
            f.traefik.get_or_insert(empty_traefik(span)).disable = Some(span);
        },
    },
    // `command`'s own collision point (#156) — the same
    // shell-string-or-exec-list shape `healthcheck.test` carries, so it
    // shares that row's `ScalarValue::List` conversion, just written into
    // `ServiceFields::command` directly rather than reached through a
    // nested struct's `get_or_insert` — the same direct-field shape
    // `container_name`'s row already has.
    ScalarField {
        key: "command",
        take: |f| {
            f.command.take().map(|command| match command {
                Command::Shell(lit) => ScalarValue::Literal(lit),
                Command::Exec(items, span) => ScalarValue::List(items, span),
            })
        },
        set: |f, v| {
            f.command = Some(match v {
                ScalarValue::Literal(lit) => Command::Shell(lit),
                ScalarValue::List(items, span) => Command::Exec(items, span),
                ScalarValue::Flag(_) => {
                    unreachable!("`command`'s own `take` never produces `ScalarValue::Flag`")
                }
            });
        },
    },
    // `entrypoint`'s own collision point (#183) — merges exactly like
    // `command` just above, in its own row: two independent Compose keys,
    // so a template setting one and a template setting the other don't
    // collide with each other, they compose (each keyed separately here,
    // same as every other row in this table).
    ScalarField {
        key: "entrypoint",
        take: |f| {
            f.entrypoint.take().map(|entrypoint| match entrypoint {
                Entrypoint::Shell(lit) => ScalarValue::Literal(lit),
                Entrypoint::Exec(items, span) => ScalarValue::List(items, span),
            })
        },
        set: |f, v| {
            f.entrypoint = Some(match v {
                ScalarValue::Literal(lit) => Entrypoint::Shell(lit),
                ScalarValue::List(items, span) => Entrypoint::Exec(items, span),
                ScalarValue::Flag(_) => {
                    unreachable!("`entrypoint`'s own `take` never produces `ScalarValue::Flag`")
                }
            });
        },
    },
    // `privileged`'s own collision point (#157) — a bare `ServiceFields`
    // field rather than one nested inside a struct, but merged exactly
    // like `healthcheck.disable`/`traefik.disable` above: a
    // `FieldKind::BoolFlag`, so its row round-trips through
    // `ScalarValue::Flag`.
    ScalarField {
        key: "privileged",
        take: |f| f.privileged.take().map(ScalarValue::Flag),
        set: |f, v| {
            let ScalarValue::Flag(span) = v else {
                unreachable!("`privileged`'s own `take` only ever produces `ScalarValue::Flag`")
            };
            f.privileged = Some(span);
        },
    },
];

/// A freshly materialized [`Healthcheck`] with every sub-field unset,
/// for the `get_or_insert` calls [`SCALAR_FIELDS`]'s `healthcheck.*`
/// rows share — factored out once so a future `Healthcheck` sub-field
/// doesn't have to be added to seven near-identical struct literals.
/// A [`Build`] with every sub-field unset, for a [`SCALAR_FIELDS`] row
/// that has to materialize one before writing its own slot — the same
/// job [`empty_healthcheck`] does for `healthcheck`.
fn empty_build(span: Span) -> Build {
    Build {
        context: None,
        dockerfile: None,
        span,
    }
}

fn empty_healthcheck(span: Span) -> Healthcheck {
    Healthcheck {
        test: None,
        interval: None,
        timeout: None,
        retries: None,
        start_period: None,
        start_interval: None,
        disable: None,
        span,
    }
}

/// A freshly materialized [`Traefik`] with `disable` unset, mirroring
/// [`empty_healthcheck`] for the same reason (#159): the one call site
/// today (`"traefik.disable"`'s own `get_or_insert`, in [`SCALAR_FIELDS`])
/// doesn't need the indirection yet, but a second `Traefik` sub-field
/// would otherwise force every existing call site to be revisited
/// instead of just gaining a new one.
fn empty_traefik(span: Span) -> Traefik {
    Traefik {
        disable: None,
        span,
    }
}

/// [`ScalarField`]'s counterpart for reference-list fields — the same
/// `key`/`take`/`set` triple, minus everything scalars need only
/// because they can collide. A list never collides (tiers concatenate,
/// per docs/DESIGN.md's composition rules), so there's no `Tier` to
/// track and no error to key by name; `key` is here purely as the
/// `MergeAcc::lists` map key, and stays the same fully-dotted canonical
/// path convention `ScalarField::key` documents. `set` is only ever
/// called with a non-empty list — see [`merge_tier`].
///
/// Introduced when `expose.entrypoint` became a list (hl-lang#73), back
/// when `entrypoint` lived inside `expose`, which
/// `MergeAcc::into_service_fields` *rebuilds* — needing the same
/// read-out/write-back indirection the scalars already had, rather than
/// the plain "name each one" every other reference list got. #198 moved
/// `entrypoint` off `expose` entirely (it lives under a router name now,
/// merged through [`MergeAcc::routers`] instead — see [`merge_routers`]),
/// so the three rows left here (`networks`/`dns`/`env_file`)
/// no longer strictly need the indirection for that original reason —
/// kept anyway since they still ride the same table [`SCALAR_FIELDS`]
/// (see hl-lang#28) exists to generalize: both merge functions stay one
/// generic loop apiece with no hand-enumerated knowledge of
/// `ServiceFields`'s shape, and a bespoke `MergeAcc` field per list would
/// be exactly the shape that design set out to remove.
struct ListField {
    key: &'static str,
    take: fn(&mut ServiceFields) -> Vec<Literal>,
    set: fn(&mut ServiceFields, Vec<Literal>),
    /// Whether repeats of an already-accumulated name are dropped
    /// rather than appended (hl-lang#69). True for the set-like fields
    /// — `networks` alone, since #221 moved `middleware` onto `router`
    /// (where [`merge_routers`] dedupes it by the same rule) — where
    /// naming
    /// the same thing twice means exactly what naming it once means, so
    /// the repeat is pure noise: it duplicated `networks:` entries and
    /// `middlewares=` label values in the output, made a single
    /// external network look like an ambiguity with itself, and (since
    /// list size then doubled per composition level) turned a few
    /// hundred bytes of nested `with` into an out-of-memory abort.
    ///
    /// `dns` and `env_file` are the two exceptions, deliberately: order
    /// is observable for both — `dns` as resolver priority, `env_file`
    /// as Compose's own last-file-wins rule when the same variable is
    /// set in two of the listed files (#154) — so their append
    /// semantics are left exactly as they were even though a repeat is
    /// just as meaningless. (`devices` used to sit in this same list,
    /// deduped like `networks` rather than kept like
    /// `dns`/`env_file` — see #157's original reasoning, superseded by
    /// #167's move onto [`merge_map`], which dedupes every map-kind
    /// field's repeats by construction: a later entry with the same key
    /// simply replaces the earlier one, so there is no separate
    /// `dedupe` flag to set for it any more.)
    dedupe: bool,
}

static LIST_FIELDS: &[ListField] = &[
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
    ListField {
        key: "env_file",
        dedupe: false,
        take: |f| std::mem::take(&mut f.env_file),
        set: |f, v| f.env_file = v,
    },
];

/// [`ScalarField`]/[`ListField`]'s counterpart for `volume`/`publish`/
/// `devices` (#192) — the same `key`/`take`/`set` triple, driving
/// [`Self::arrow_maps`]'s single [`HashMap`] bucket the way
/// [`SCALAR_FIELDS`]/[`LIST_FIELDS`] already drive [`Self::scalars`]/
/// [`Self::lists`]. All three fields merge through the identical
/// [`merge_map`] call — same [`crate::schema::MapSide::Value`]
/// uniqueness side, same `|e| e.container.text().to_string()` key — so
/// [`merge_tier`] and [`MergeAcc::into_service_fields`] each need only
/// loop over this table instead of repeating that call three times by
/// hand. `env` isn't a row here even though it also merges through
/// [`merge_map`]: it keys on [`crate::schema::MapSide::Key`] instead, and
/// its entries are [`EnvEntry`] rather than [`crate::ast::ArrowMapEntry`]
/// — it stays its own [`MergeAcc::env`] field and its own direct
/// `merge_map` call in [`merge_tier`].
struct ArrowMapField {
    key: &'static str,
    take: fn(&mut ServiceFields) -> ArrowMap,
    set: fn(&mut ServiceFields, ArrowMap),
}

static ARROW_MAP_FIELDS: &[ArrowMapField] = &[
    ArrowMapField {
        key: "volume",
        take: |f| std::mem::take(&mut f.volumes),
        set: |f, v| f.volumes = v,
    },
    ArrowMapField {
        key: "publish",
        take: |f| std::mem::take(&mut f.publish),
        set: |f, v| f.publish = v,
    },
    ArrowMapField {
        key: "devices",
        take: |f| std::mem::take(&mut f.devices),
        set: |f, v| f.devices = v,
    },
];

/// Merges one tier's [`ServiceFields`] into `acc`. Every [`LIST_FIELDS`]
/// entry concatenates rather than collides — the set-like reference
/// lists concatenate *by distinct name*, dropping a repeat of a name an
/// earlier tier (or an earlier entry of the same list) already
/// contributed; see [`ListField::dedupe`] for which fields those are
/// and why `dns`/`env_file` aren't among them. `raw` isn't one of them
/// any more (#193) — it merges key-by-key through [`merge_map`] exactly
/// like `env`, so two explicit templates setting the same `raw` key
/// collide instead of the second one silently winning. A `raw` key
/// repeated *within* one body is the parser's own business, in
/// `merge_raw_entries` — a separate code path this function never
/// touches, which #206 brought to the same rule.
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
    // `healthcheck.test`/`.disable`, `traefik.disable`, `command`,
    // `entrypoint`, and `privileged` all rode their own dedicated
    // `MergeAcc` slot through a second generic function,
    // `merge_scalar_like`, before #197 — none of them are `Literal`-valued,
    // so none could ride `SCALAR_FIELDS`'s table as it stood. They're
    // ordinary rows in that same table now (see [`ScalarValue`]'s doc for
    // how), so the loop just above already merges all six; there is
    // nothing left to do for them here.
    //
    // Before the `merge_map` calls below only because those consume
    // `incoming`'s map entries by value, and `take` needs `incoming`
    // whole; the merge itself is order-independent.
    //
    // The emptiness check is load-bearing, not a micro-optimization:
    // it's what establishes `acc.lists`'s invariant that a key present
    // in the map always maps to a non-empty list — `into_service_fields`
    // relies on it, since a `set` writes the whole merged list back in
    // one call rather than accumulating into it. Deduping can't undermine
    // that: a non-empty `values` whose every entry is dropped as a
    // repeat can only happen when the accumulated list already held
    // those names, i.e. was already non-empty.
    for field in LIST_FIELDS {
        let values = (field.take)(&mut incoming);
        if !values.is_empty() {
            let acc_values = acc.lists.entry(field.key).or_default();
            if field.dedupe {
                // First occurrence wins, so the accumulated order is
                // still tier order (each `with` target left-to-right,
                // then the body's own list) with later
                // repeats dropped — see [`ListField::dedupe`]. The
                // linear scan is over a list whose length is now bounded
                // by the number of *distinct* names, which is what makes
                // this cheap and is the whole reason #69's exponential
                // blowup stops here.
                //
                // Comparing by `Literal::text` alone is right because a
                // qualified `networks [alias.name]` entry has already
                // been rewritten to its resolved bare name by
                // [`resolve_qualified_references`] before any tier reaches
                // this function, and the other deduped fields reject
                // qualifiers outright.
                for value in values {
                    if !acc_values.iter().any(|held| held.text() == value.text()) {
                        acc_values.push(value);
                    }
                }
            } else {
                acc_values.extend(values);
            }
        }
    }
    // `volume`/`publish`/`devices` (#192): all three key on the container
    // side and merge identically, so one loop over [`ARROW_MAP_FIELDS`]
    // replaces what used to be three hand-written `merge_map` calls —
    // see that table's own doc, and `schema::DEVICES`'s for why `devices`
    // shares `publish`'s container-side uniqueness convention.
    for field in ARROW_MAP_FIELDS {
        let entries = (field.take)(&mut incoming).entries;
        if !entries.is_empty() {
            merge_map(
                acc.arrow_maps.entry(field.key).or_default(),
                field.key,
                MapSide::Value,
                entries,
                tier,
                |e| e.container.text().to_string(),
            )?;
        }
    }
    merge_map(
        &mut acc.env,
        "env",
        MapSide::Key,
        incoming.env.entries,
        tier,
        |e| e.key.text().to_string(),
    )?;
    // Keyed exactly like `env` above — same side, same rules (#243). The
    // accumulated order is tier order (each `with` target left to
    // right, then the body's own), which is what makes
    // the emitted label order a stable function of the source.
    merge_map(
        &mut acc.labels,
        "labels",
        MapSide::Key,
        incoming.labels.entries,
        tier,
        |e| e.key.text().to_string(),
    )?;
    // Keyed by the referenced service's own name, like `env`'s key
    // side — not concatenated through `LIST_FIELDS` above, even though
    // its surface syntax is still a comma/bracket list. Not plain
    // `merge_map` either, unlike `volumes`/`env`/`publish` just above:
    // see `merge_depends_on`'s own doc for the narrower collision rule
    // this field needs.
    merge_depends_on(&mut acc.depends_on, incoming.depends_on, tier)?;
    // Keyed by router name, then merged sub-field by sub-field within
    // each key — see `merge_routers`' own doc for why neither
    // `merge_map` nor `LIST_FIELDS` fits (#184).
    merge_routers(&mut acc.routers, incoming.routers, tier)?;
    // Keyed like `env` — same [`MapSide::Key`] uniqueness convention —
    // now that `raw` isn't the language's one unconditionally
    // concatenated map field any more (#193). See `MergeAcc::raw`'s doc.
    merge_map(
        &mut acc.raw,
        "raw",
        MapSide::Key,
        incoming.raw.entries,
        tier,
        |e| e.key.text().to_string(),
    )?;
    Ok(())
}

/// Merges `router` blocks into `acc`, keyed by router name (#184).
///
/// Two levels of merging, not one. Between routers, this is keyed like
/// [`merge_map`]: a name no tier has contributed yet is appended, and a
/// name an earlier tier already contributed is merged into rather than
/// added twice. *Within* one name, each sub-field then merges by its own
/// kind, exactly the way `expose`'s `port`/`host`/`entrypoint` do — the
/// scalar `host` follows [`merge_scalar`]'s Own-always-wins /
/// two-explicit-templates-collide rule, while
/// `entrypoints`, `path_prefix`, and `middleware` concatenate.
///
/// That second level is the whole point. [`merge_map`]'s own
/// full-entry replacement is right for `volume`/`publish`, where an
/// entry is a single mapping with nothing inside it to keep, but a
/// router is a record: a service body writing `router api { host: "..."
/// }` over a template's `router api { entrypoints: web-secure }` means
/// "same router, different host," not "throw the entry point away." So
/// this reads as the keyed form of the per-sub-field merge
/// docs/DESIGN.md already describes for `expose`.
///
/// `entrypoints` and `middleware` dedupe by name and `path_prefix`
/// doesn't, matching what each list means: naming one entry point or one
/// middleware twice attaches the router to it once, while path prefixes
/// are `||` alternatives whose written order is observable in the
/// emitted rule — the same split [`ListField::dedupe`] already draws
/// between `networks` and `dns`/`env_file`. `middleware` dedupes on the
/// side `networks` sits on, for the same reason: a repeated middleware
/// name would be a repeated entry in the one comma-joined
/// `middlewares=` label.
///
/// A router's `middleware` merging across tiers this way — rather than
/// the innermost tier replacing what an outer one said — is what lets a
/// template supply a base list a service body adds to (#221), the same
/// as `entrypoints` beside it.
fn merge_routers(
    acc: &mut Vec<RouterAcc>,
    incoming: Vec<Router>,
    tier: &Tier,
) -> Result<(), ComposeError> {
    for router in incoming {
        let key = router.key().map(str::to_string);
        let Some(pos) = acc.iter().position(|held| held.key() == key.as_deref()) else {
            acc.push(RouterAcc {
                name: router.name,
                host: router.host.map(|h| (h, tier.clone())),
                entrypoints: router.entrypoints,
                path_prefix: router.path_prefix,
                middleware: router.middleware,
                priority: router.priority.map(|p| (p, tier.clone())),
                port: router.port.map(|p| (p, tier.clone())),
                protocol: router.protocol.map(|p| (p, tier.clone())),
                rule: router.rule.map(|r| (r, tier.clone())),
                span: router.span,
            });
            continue;
        };
        if let Some(host) = router.host {
            merge_router_scalar(
                &mut acc[pos].host,
                "router.host",
                key.as_deref(),
                host,
                tier,
            )?;
        }
        // #225's three scalars follow `host`'s rule exactly, keyed the
        // same way so a collision still says *which* router.
        if let Some(priority) = router.priority {
            merge_router_scalar(
                &mut acc[pos].priority,
                "router.priority",
                key.as_deref(),
                priority,
                tier,
            )?;
        }
        if let Some(port) = router.port {
            merge_router_scalar(
                &mut acc[pos].port,
                "router.port",
                key.as_deref(),
                port,
                tier,
            )?;
        }
        if let Some(protocol) = router.protocol {
            merge_router_scalar(
                &mut acc[pos].protocol,
                "router.protocol",
                key.as_deref(),
                protocol,
                tier,
            )?;
        }
        // #228: a whole rule, merged by the same rule the scalars are.
        if let Some(rule) = router.rule {
            merge_router_scalar(
                &mut acc[pos].rule,
                "router.rule",
                key.as_deref(),
                rule,
                tier,
            )?;
        }
        // First occurrence wins, so accumulated order stays tier order
        // with later repeats dropped — the same dedupe-by-name rule
        // `LIST_FIELDS`'s set-like rows follow (see [`ListField::dedupe`]),
        // reached through the same comparison on `Literal::text` (a
        // qualified entry can never get this far: it's rejected outright
        // by `resolve_qualified_references`).
        for entry in router.entrypoints {
            if !acc[pos]
                .entrypoints
                .iter()
                .any(|held| held.text() == entry.text())
            {
                acc[pos].entrypoints.push(entry);
            }
        }
        acc[pos].path_prefix.extend(router.path_prefix);
        // Deduped by name like `entrypoints` just above, and for the same
        // reason — see this function's own doc (#221).
        for entry in router.middleware {
            if !acc[pos]
                .middleware
                .iter()
                .any(|held| held.text() == entry.text())
            {
                acc[pos].middleware.push(entry);
            }
        }
        // The most recent contributor's span, so a diagnostic about the
        // merged router points at the most specific place it was
        // written — the service's own body when it wrote one, the
        // template otherwise.
        acc[pos].span = router.span;
    }
    Ok(())
}

/// [`merge_scalar`]'s rule applied to one of a router's own scalar
/// sub-fields, reported as a [`ComposeError::MapKeyCollision`] rather
/// than a [`ComposeError::FieldCollision`] because the field alone
/// (`router.host`) doesn't say *which* router collided — the key does,
/// and `MapKeyCollision` is the variant that already carries one.
///
/// Generic over the slot rather than written once per sub-field: `host`
/// was the only one until #225 added `priority`/`port`/`protocol`, and
/// all four want the identical Own-wins /
/// two-explicit-templates-collide rule. `slot` is the [`RouterAcc`]
/// field to merge into and `field` the dotted name a collision reports.
fn merge_router_scalar<T: RouterScalar>(
    slot: &mut Option<(T, Tier)>,
    field: &'static str,
    key: Option<&str>,
    value: T,
    tier: &Tier,
) -> Result<(), ComposeError> {
    match slot.take() {
        None => *slot = Some((value, tier.clone())),
        Some((existing, existing_tier)) => match (&existing_tier, tier) {
            (_, Tier::Own) => *slot = Some((value, Tier::Own)),
            (Tier::Explicit(first), Tier::Explicit(second)) => {
                return Err(ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
                    field,
                    side: MapSide::Key,
                    key: router_key_display(key),
                    first_template: first.clone(),
                    second_template: second.clone(),
                    first: existing.span(),
                    second: value.span(),
                })));
            }
            _ => unreachable!("Own is always merged last, so it is never the existing tier"),
        },
    }
    Ok(())
}

/// A single-occurrence `router` field [`merge_router_scalar`] can merge.
///
/// The merge itself is the same for every one of them — Own always wins,
/// two explicit templates collide — and the
/// only thing it needs from the value is where it was written, for the
/// collision diagnostic to name both sides. Six of the seven rows are
/// [`Literal`]s; `rule` (#228) is a whole [`MatchExpr`], which is the
/// only reason this is a trait rather than a concrete type.
trait RouterScalar {
    fn span(&self) -> Span;
}

impl RouterScalar for Literal {
    fn span(&self) -> Span {
        Literal::span(self)
    }
}

impl RouterScalar for MatchExpr {
    fn span(&self) -> Span {
        MatchExpr::span(self)
    }
}

/// Merges `depends_on` entries into `acc`, keyed on the referenced
/// service's own name — almost [`merge_map`], but with one narrower
/// twist on the two-`Explicit`-tiers-collide case (#155).
///
/// `merge_map`'s own rule collides on key equality alone, which is
/// right for `env`/`volume`/`publish`: two explicit templates setting
/// the same key are colliding on that key even if they happen to write
/// the same *value*, because nothing forces them to agree, and there's
/// no principled reason to let today's accidental agreement paper over
/// tomorrow's real one. `depends_on` is different: Compose's own
/// implicit default already fixes what a *bare* entry means
/// (`service_started`), so two explicit templates each writing
/// `depends_on [db]` — the overwhelmingly common case — aren't
/// proposing two different answers that happen to coincide, they're
/// giving the *same* answer twice. Erroring there would be a gratuitous
/// break of every `.hll` file that already composed two templates each
/// depending on the same service, for a "conflict" that was never one —
/// the same reasoning `resolve_networks`'s `AmbiguousExternalNetwork`
/// check already applies to a network named `external` by two
/// declarations that resolve to the same real name: "naming one
/// external network more than once is not an ambiguity between it and
/// itself, it's one answer given twice."
///
/// So two entries naming the same service are compared by
/// [`DependsOnEntry::effective_condition`] — which folds a bare entry
/// into Compose's own `service_started` default before comparing —
/// before ever reaching the collision check: equal, and the earlier
/// entry's own *written* form is kept, exactly like the set-like lists'
/// own first-occurrence-wins dedupe; unequal, and it's a genuine
/// [`ComposeError::MapKeyCollision`], the same diagnostic `env`/
/// `volume`/`publish` raise for their own key collisions. Keeping the
/// earlier entry's written form rather than normalizing it to whichever
/// of the two conditions won matters for codegen: whether *any* entry
/// in the field carries an explicit `condition` at all is what selects
/// Compose's short-vs-long `depends_on:` shape (see
/// `hl_codegen::generate_depends_on`), so silently promoting a bare
/// entry into an explicit `service_started` here would flip an
/// otherwise all-bare `depends_on` field into the long map form for no
/// reason any `.hll` file actually wrote.
fn merge_depends_on(
    acc: &mut Vec<(DependsOnEntry, Tier)>,
    incoming: Vec<DependsOnEntry>,
    tier: &Tier,
) -> Result<(), ComposeError> {
    for entry in incoming {
        let key = entry.reference.text().to_string();
        if let Some(pos) = acc.iter().position(|(e, _)| e.reference.text() == key) {
            let existing_tier = acc[pos].1.clone();
            match (&existing_tier, tier) {
                (_, Tier::Own) => {
                    acc[pos] = (entry, Tier::Own);
                }
                (Tier::Explicit(first), Tier::Explicit(second)) => {
                    if acc[pos].0.effective_condition() == entry.effective_condition() {
                        // Same answer, given twice — not a collision;
                        // keep the earlier entry's own written form (see
                        // this function's own doc) and drop the repeat.
                        continue;
                    }
                    return Err(ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
                        field: "depends_on",
                        side: MapSide::Key,
                        key,
                        first_template: first.clone(),
                        second_template: second.clone(),
                        first: acc[pos].0.span,
                        second: entry.span,
                    })));
                }
                _ => unreachable!("Own is always merged last, so it is never the existing tier"),
            }
        } else {
            acc.push((entry, tier.clone()));
        }
    }
    Ok(())
}

/// Merges one scalar (or scalar-*like*, see [`ScalarValue`]) collision
/// point, keyed by `field` (e.g. `"expose.port"`, `"healthcheck.test"`,
/// `"privileged"`), into `acc`. `Own` always wins unconditionally; two
/// `Explicit` tiers setting the same key is a compile error. The single merge routine
/// every scalar-shaped field in the language goes through — see
/// [`MergeAcc`]'s own doc for why this replaced the old
/// `Spanned`-generic, one-slot-per-field `merge_single`, and
/// [`ScalarValue`]'s for why a second generic function
/// (`merge_scalar_like`, folded into this one at #197) isn't needed any
/// more to cover the collision points whose slot isn't a plain
/// [`Literal`].
fn merge_scalar(
    acc: &mut HashMap<&'static str, (ScalarValue, Tier)>,
    field: &'static str,
    value: ScalarValue,
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
            (Tier::Explicit(first), Tier::Explicit(second)) => {
                return Err(ComposeError::FieldCollision {
                    field,
                    first_template: first.clone(),
                    second_template: second.clone(),
                    first: existing.span(),
                    second: value.span(),
                });
            }
            _ => unreachable!("Own is always merged last, so it is never the existing tier"),
        },
    }
    Ok(())
}

/// Merges one map-kind field's entries, keyed by `key_of` (the container
/// path for `volume`, the key for `env`, the container port for
/// `publish` — matching each field's existing
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
                _ => unreachable!("Own is always merged last, so it is never the existing tier"),
            }
        } else {
            acc.push((entry, tier.clone()));
        }
    }
    Ok(())
}
