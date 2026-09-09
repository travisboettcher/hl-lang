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
    EnvMap, Expose, FieldAccess, Healthcheck, HealthcheckTest, Ident, Image, LabelEntry, LabelMap,
    LabelValue, Literal, Network, Program, QualifiedRef, RawEntry, RawMap, RawValue, Restart,
    Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, Volume,
};
use crate::interp;
use crate::schema::{self, MapSide};
use crate::warning::ComposeWarning;

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
    /// Every non-fatal diagnostic composition raised on the way here, in
    /// the order it was raised (see [`ComposeWarning`]).
    ///
    /// Rides *inside* [`ComposedProgram`] rather than alongside it the
    /// way `hl_linker`'s own warnings do, because unlike those there is
    /// no wrapper type at this layer to put them in: [`compose`] returns
    /// the program itself, and a second return value would have to be
    /// threaded through `hl_linker::link` — which produces this same
    /// type — to reach `hllc` at all.
    pub warnings: Vec<ComposeWarning>,
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
    /// (`networks`, `dns`, `env_file`, a `depends_on` entry's own
    /// reference) resolved to a bare number. #201 dropped
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
    /// A `{{param}}` interpolation whose bound argument has no string
    /// form to splice into the surrounding content: a list, a nested
    /// map, or an `alias.name` qualified reference.
    ///
    /// The first two are [`Self::TemplateArgumentNotScalar`]'s case one
    /// step further along — there a list can't *fill* a single-value
    /// slot, here it can't fill part of one. The qualified case is a
    /// backstop rather than a reachable diagnostic today: an argument
    /// body's grammar has no place for an `alias.name`, so one can't be
    /// passed to interpolate in the first place. Named anyway, because
    /// the reason it *would* be rejected is a property of qualified
    /// references and not of the grammar that currently excludes them:
    /// the qualifier is an import alias, which exists only while
    /// composition is resolving names and means nothing in generated
    /// output, so there is no honest text to substitute (rendering it
    /// `alias.name` would emit a local alias the deployment never
    /// sees).
    ///
    /// Names the argument at its own call site rather than the `{{...}}`
    /// use site, matching [`Self::ArgumentNotReferenceShaped`] and
    /// [`Self::ArgumentNotNumeric`]: the argument is what has to change,
    /// and one template's body can be reached from many call sites.
    ArgumentNotInterpolable {
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
    /// A field access whose base names nothing this program declares —
    /// `proxy.name` with no `network proxy` and no `volume proxy`
    /// anywhere in it (#275).
    ///
    /// A bare base is resolved against one flat namespace of the
    /// program's own `network` and `volume` declarations, exactly as a
    /// `networks [proxy]` entry is at codegen: an imported declaration
    /// is reached by naming its alias (`traefik.proxy.name`), which is
    /// what the message points at, since that spelling also says which
    /// file the name is expected to come from.
    FieldBaseNotDeclared { base: String, span: Span },
    /// A field access whose base names something real that has no
    /// fields to read — a `service` (#275).
    ///
    /// Told apart from [`Self::FieldBaseNotDeclared`] because the fix is
    /// a different one: nothing is misspelled and nothing is missing,
    /// the name simply belongs to a kind of declaration that carries no
    /// Docker name of its own. A service's name *is* its Compose key,
    /// with no override to read.
    FieldBaseNotDeclaration {
        base: String,
        found: &'static str,
        span: Span,
    },
    /// A field access naming a field the declaration doesn't have —
    /// `proxy.driver`, `media.external` (#275).
    ///
    /// `name` is the whole of what a declaration exposes, deliberately:
    /// it's the one thing a `network`/`volume` knows that a value
    /// position can't already write for itself, and every other setting
    /// on one (`external`, `driver`, `driver_opts`) describes how Docker
    /// should make the thing rather than naming it. The message says so
    /// rather than only refusing, since the whole available set is one
    /// word long.
    UnknownDeclarationField {
        kind: &'static str,
        decl: String,
        field: String,
        /// What this kind *does* expose — the declaration kind's own
        /// list of readable fields, carried so the message
        /// can list it rather than restating one kind's fields in prose
        /// that the next kind would contradict.
        readable: &'static [&'static str],
        span: Span,
    },
    /// A field the declaration's kind has, that this declaration leaves
    /// unset — a `volume`'s `driver` when it lets Docker choose (#275).
    ///
    /// Its own error rather than an empty string, because there is no
    /// honest text for "whatever Docker picks" to splice into a label,
    /// and no reason to think the user wanted the empty one.
    DeclarationFieldUnset {
        kind: &'static str,
        decl: String,
        field: String,
        span: Span,
    },
    /// A field that exists on the declaration but never holds a value:
    /// a bare-presence flag such as `external`, or a nested map such as
    /// a `volume`'s `driver_opts` (#275).
    ///
    /// Distinct from [`Self::UnknownDeclarationField`] because the field
    /// is real — saying "no such field" of something written three lines
    /// up sends the reader looking for a typo that isn't there.
    DeclarationFieldNotAValue {
        kind: &'static str,
        decl: String,
        field: String,
        /// Why it holds no value, phrased to sit after "is".
        what: &'static str,
        span: Span,
    },
    /// A field access whose alias resolved to a real imported scope,
    /// but that scope declares no `network` *or* `volume` under the
    /// base's name (#275).
    ///
    /// The field-access counterpart of
    /// [`Self::UnknownQualifiedNetwork`]/[`Self::UnknownQualifiedVolume`],
    /// which each answer for one kind because the position that raised
    /// them (`networks [...]`, a named-volume mount) can only mean that
    /// kind. `traefik.proxy.name` names neither kind in particular —
    /// both carry a `name` — so it reports the one question that was
    /// actually asked.
    UnknownQualifiedDeclaration {
        alias: String,
        name: String,
        span: Span,
    },
    /// A `$param.name` whose bound argument can't carry a field: a
    /// number, a list, a nested map (#275).
    ///
    /// Names the argument at its own call site, matching
    /// [`Self::ArgumentNotReferenceShaped`] and
    /// [`Self::ArgumentNotNumeric`]: substitution overwrites the base
    /// slot, span included, so what's left to report is the caller's own
    /// literal — which is also the thing that has to change, since one
    /// template body is reached from many call sites.
    ArgumentCantCarryField {
        template: String,
        param: String,
        found: &'static str,
        span: Span,
    },
    /// A `with`-invocation argument naming an imported declaration
    /// (`with t { net: shared.proxy }`, #296) that the template then
    /// uses somewhere a plain value belongs — an `env` value, a label,
    /// an `image` — rather than as a reference or a field-access base.
    ///
    /// A declaration reference has no text form a value position could
    /// take: `networks`/a named-volume host attach the declaration, and
    /// `{{net.name}}`/`$net.name` read a field off it, but an `env`
    /// value has nothing to do with either. Raised at the argument's own
    /// span — substitution copies the caller's literal wholesale, span
    /// included — for [`Self::ArgumentCantCarryField`]'s reason: the
    /// argument is the half that has to change.
    QualifiedArgumentNotAValue {
        template: String,
        alias: String,
        name: String,
        span: Span,
    },
    /// An interpolated field access with more dotted segments than any
    /// of its shapes has — `"{{a.b.c.d}}"` (#275).
    ///
    /// [`crate::ParseError::FieldAccessTooDeep`]'s twin for the
    /// interpolated spelling, which the parser never sees: `{{...}}` is
    /// ordinary string content, so arity is only counted once
    /// composition scans it.
    InterpolatedFieldAccessTooDeep { text: String, span: Span },
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
    /// One `labels` key written as a list in one place and a single
    /// value in another (#288).
    ///
    /// The two disagree about what the key holds, and neither resolution
    /// is honest: concatenating a scalar into a list invents a list the
    /// author didn't write, and letting either win silently discards the
    /// other. The shapes mean different things about merging — a list
    /// composes across tiers, a scalar collides — so this is a real
    /// disagreement rather than a formatting difference.
    LabelShapeMismatch {
        key: String,
        first_shape: &'static str,
        second_shape: &'static str,
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
    /// field that has no cross-file meaning — `depends_on`, `dns`, or
    /// `env_file`. (`depends_on` names
    /// a same-file sibling service; the others aren't resolved against
    /// anything an `.hll` file declares at all — an `env_file` path lives
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
            | ComposeError::ArgumentNotInterpolable { span, .. }
            | ComposeError::FieldNotNumeric { span, .. }
            | ComposeError::FieldBaseNotDeclared { span, .. }
            | ComposeError::FieldBaseNotDeclaration { span, .. }
            | ComposeError::UnknownDeclarationField { span, .. }
            | ComposeError::DeclarationFieldUnset { span, .. }
            | ComposeError::DeclarationFieldNotAValue { span, .. }
            | ComposeError::UnknownQualifiedDeclaration { span, .. }
            | ComposeError::ArgumentCantCarryField { span, .. }
            | ComposeError::QualifiedArgumentNotAValue { span, .. }
            | ComposeError::InterpolatedFieldAccessTooDeep { span, .. }
            | ComposeError::FieldCollision { second: span, .. }
            | ComposeError::UnknownAlias { span, .. }
            | ComposeError::UnsupportedQualifiedReference { span, .. }
            | ComposeError::UnknownQualifiedNetwork { span, .. }
            | ComposeError::CollidingImportedNetwork { span, .. }
            | ComposeError::UnknownQualifiedVolume { span, .. }
            | ComposeError::CollidingImportedVolume { span, .. } => *span,
            ComposeError::MapKeyCollision(details) => details.second,
            ComposeError::LabelShapeMismatch { second, .. } => *second,
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
            ComposeError::ArgumentNotInterpolable {
                template,
                param,
                found,
                ..
            } => write!(
                f,
                "{at}: argument `{param}` for template `{template}` can't be interpolated into \
                 a string (found {found})"
            ),
            ComposeError::FieldNotNumeric { field, found, .. } => {
                write!(f, "{at}: `{field}` must be a number (found {found})")
            }
            ComposeError::FieldBaseNotDeclared { base, .. } => write!(
                f,
                "{at}: `{base}` names no `network` or `volume` declared in this program, so \
                 `{base}.name` has no name to read — declare one, or write \
                 `alias.{base}.name` to read an imported declaration"
            ),
            ComposeError::FieldBaseNotDeclaration { base, found, .. } => write!(
                f,
                "{at}: `{base}` is {found}, and only a `network` or `volume` declaration \
                 carries a name to read — name one of those instead"
            ),
            ComposeError::UnknownDeclarationField {
                kind,
                decl,
                field,
                readable,
                ..
            } => write!(
                f,
                "{at}: `{kind} {decl}` has no field `{field}` — a {kind} exposes {}",
                readable_field_list(readable)
            ),
            ComposeError::DeclarationFieldUnset {
                kind, decl, field, ..
            } => write!(
                f,
                "{at}: `{kind} {decl}` sets no `{field}`, so there is no value to read — set one \
                 on the declaration, or write the value here directly"
            ),
            ComposeError::DeclarationFieldNotAValue {
                kind,
                decl,
                field,
                what,
                ..
            } => write!(
                f,
                "{at}: `{kind} {decl}`'s `{field}` is {what}, so it can't fill a value here"
            ),
            ComposeError::UnknownQualifiedDeclaration { alias, name, .. } => {
                write!(f, "{at}: no network or volume `{name}` in `{alias}`")
            }
            ComposeError::ArgumentCantCarryField {
                template,
                param,
                found,
                ..
            } => write!(
                f,
                "{at}: argument `{param}` for template `{template}` must name a `network` or \
                 `volume` declaration to read a field off it (found {found})"
            ),
            ComposeError::QualifiedArgumentNotAValue {
                template,
                alias,
                name,
                ..
            } => write!(
                f,
                "{at}: `{alias}.{name}` names a declaration in `{alias}`, and template \
                 `{template}` uses it where a plain value belongs — a declaration reference can \
                 only be attached by `networks`, or read from (`{{{{param.name}}}}`), so pass \
                 the field itself (`{alias}.{name}.name`) instead"
            ),
            ComposeError::InterpolatedFieldAccessTooDeep { text, .. } => write!(
                f,
                "{at}: `{{{{{text}}}}}` has too many parts to be a field access — write \
                 `{{{{declaration.name}}}}`, or `{{{{alias.declaration.name}}}}` for an \
                 imported declaration"
            ),
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
            ComposeError::LabelShapeMismatch {
                key,
                first_shape,
                second_shape,
                first,
                ..
            } => write!(
                f,
                "{at}: `labels` key {key:?} is {second_shape} here but {first_shape} at {} — a \
                 list composes across templates and a single value doesn't, so the two say \
                 different things about what this key holds",
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

    /// Offers the qualified spelling by name, since a base that names
    /// nothing locally is most often one that lives in an imported file
    /// (#275).
    #[test]
    fn field_base_not_declared_display() {
        let err = ComposeError::FieldBaseNotDeclared {
            base: "proxy".to_string(),
            span: span(5, 17),
        };
        assert_eq!(
            err.to_string(),
            "5:17: `proxy` names no `network` or `volume` declared in this program, so \
             `proxy.name` has no name to read — declare one, or write `alias.proxy.name` to \
             read an imported declaration"
        );
    }

    #[test]
    fn field_base_not_declaration_display() {
        let err = ComposeError::FieldBaseNotDeclaration {
            base: "jellyfin".to_string(),
            found: "a service",
            span: span(4, 10),
        };
        assert_eq!(
            err.to_string(),
            "4:10: `jellyfin` is a service, and only a `network` or `volume` declaration \
             carries a name to read — name one of those instead"
        );
    }

    /// The available set is one word long, so the message names it
    /// rather than only refusing the one that was written.
    #[test]
    fn unknown_declaration_field_display() {
        let err = ComposeError::UnknownDeclarationField {
            kind: "network",
            decl: "proxy".to_string(),
            field: "ports".to_string(),
            readable: Network::READABLE_FIELDS,
            span: span(7, 16),
        };
        assert_eq!(
            err.to_string(),
            "7:16: `network proxy` has no field `ports` — a network exposes `name`"
        );
    }

    /// The same message over a kind with more than one readable field,
    /// since a list is where a hand-written sentence would have gone
    /// stale the moment a field was added.
    #[test]
    fn unknown_declaration_field_lists_every_readable_field() {
        let err = ComposeError::UnknownDeclarationField {
            kind: "volume",
            decl: "media".to_string(),
            field: "ports".to_string(),
            readable: Volume::READABLE_FIELDS,
            span: span(7, 16),
        };
        assert_eq!(
            err.to_string(),
            "7:16: `volume media` has no field `ports` — a volume exposes `name` and `driver`"
        );
    }

    /// A field the kind has but the declaration leaves unset reads
    /// differently from one the kind hasn't got: the fix is on the
    /// declaration, not on the spelling.
    #[test]
    fn declaration_field_unset_display() {
        let err = ComposeError::DeclarationFieldUnset {
            kind: "volume",
            decl: "media".to_string(),
            field: "driver".to_string(),
            span: span(4, 20),
        };
        assert_eq!(
            err.to_string(),
            "4:20: `volume media` sets no `driver`, so there is no value to read — set one on \
             the declaration, or write the value here directly"
        );
    }

    /// And a field that never holds a value says so, rather than
    /// claiming a field written three lines up doesn't exist.
    #[test]
    fn declaration_field_not_a_value_display() {
        let err = ComposeError::DeclarationFieldNotAValue {
            kind: "network",
            decl: "proxy".to_string(),
            field: "external".to_string(),
            what: PRESENCE_FLAG,
            span: span(9, 11),
        };
        assert_eq!(
            err.to_string(),
            "9:11: `network proxy`'s `external` is a bare-presence flag rather than a value, so \
             it can't fill a value here"
        );
    }

    /// Names both kinds, because a field access asks for either — see
    /// the variant's own doc for why this isn't
    /// `UnknownQualifiedNetwork`.
    #[test]
    fn unknown_qualified_declaration_display() {
        let err = ComposeError::UnknownQualifiedDeclaration {
            alias: "traefik".to_string(),
            name: "proxy".to_string(),
            span: span(6, 10),
        };
        assert_eq!(
            err.to_string(),
            "6:10: no network or volume `proxy` in `traefik`"
        );
    }

    #[test]
    fn argument_cant_carry_field_display() {
        let err = ComposeError::ArgumentCantCarryField {
            template: "caddy".to_string(),
            param: "net".to_string(),
            found: "a number",
            span: span(12, 15),
        };
        assert_eq!(
            err.to_string(),
            "12:15: argument `net` for template `caddy` must name a `network` or `volume` \
             declaration to read a field off it (found a number)"
        );
    }

    /// Renders the binding with its braces, since that's how it was
    /// written and what distinguishes this from the parser's own
    /// too-deep diagnostic.
    #[test]
    fn interpolated_field_access_too_deep_display() {
        let err = ComposeError::InterpolatedFieldAccessTooDeep {
            text: "a.b.c.d".to_string(),
            span: span(9, 12),
        };
        assert_eq!(
            err.to_string(),
            "9:12: `{{a.b.c.d}}` has too many parts to be a field access — write \
             `{{declaration.name}}`, or `{{alias.declaration.name}}` for an imported \
             declaration"
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
    fn qualified_argument_not_a_value_display() {
        let err = ComposeError::QualifiedArgumentNotAValue {
            template: "linuxserver_app".to_string(),
            alias: "shared".to_string(),
            name: "proxy".to_string(),
            span: span(9, 22),
        }
        .to_string();
        assert_eq!(
            err,
            "9:22: `shared.proxy` names a declaration in `shared`, and template \
             `linuxserver_app` uses it where a plain value belongs — a declaration reference \
             can only be attached by `networks`, or read from (`{{param.name}}`), so pass the \
             field itself (`shared.proxy.name`) instead"
        );
    }

    #[test]
    fn unsupported_qualified_reference_display() {
        let err = ComposeError::UnsupportedQualifiedReference {
            field: "dns",
            alias: "traefik".to_string(),
            span: span(1, 3),
        }
        .to_string();
        assert_eq!(
            err,
            "1:3: `dns` doesn't support a qualified reference yet (`traefik.` ...)"
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
    let mut warnings = Vec::new();
    let mut composed = Vec::with_capacity(services.len());
    // Collected before the services are consumed below, and only so
    // that a field access whose base names one of them can say so — see
    // [`Declarations::services`].
    let service_names: Vec<String> = services.iter().map(|s| s.name.name.clone()).collect();
    let symbols = Symbols {
        resolver,
        decls: Declarations {
            networks: &networks,
            volumes: &volumes,
            services: &service_names,
        },
    };
    for service in services {
        composed.push(compose_service(
            service,
            entry_scope,
            &symbols,
            &mut cache,
            &mut imports,
            &mut warnings,
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
        warnings,
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
///
/// It doubles as the pair of declarations a field access can read a
/// field off (#275) — the same two kinds, for the same underlying
/// reason: they're the two an `.hll` file declares that Docker knows
/// under a name of its own.
trait ImportableDecl: Clone + PartialEq {
    /// How this kind of declaration is spelled in a diagnostic, and in
    /// the source that declares one.
    const KIND: &'static str;
    /// Every field of this kind a value position can read, in the order
    /// a diagnostic should list them. What makes a field readable is
    /// that it *has* a value: a bare-presence flag and a nested map
    /// have nothing a string position could hold, so they answer
    /// [`FieldValue::NotAValue`] rather than appearing here.
    const READABLE_FIELDS: &'static [&'static str];
    /// What this declaration is called, and what a Compose section keys
    /// it under.
    fn decl_name(&self) -> &str;
    /// What `field` holds on this declaration, or `None` when this kind
    /// has no such field at all. The three answers are distinct on
    /// purpose: "no such field", "a field you left unset", and "a field
    /// that never holds a value" are three different mistakes with three
    /// different fixes.
    fn read_field(&self, field: &str) -> Option<FieldValue>;
    /// The error to raise when a different declaration already holds
    /// this bare name.
    fn collision(alias: String, name: String, span: Span) -> ComposeError;
}

impl ImportableDecl for Network {
    const KIND: &'static str = "network";

    const READABLE_FIELDS: &'static [&'static str] = &["name"];

    fn decl_name(&self) -> &str {
        &self.name.name
    }

    fn read_field(&self, field: &str) -> Option<FieldValue> {
        match field {
            "name" => Some(FieldValue::Set(self.docker_name().to_string())),
            "external" => Some(FieldValue::NotAValue(PRESENCE_FLAG)),
            _ => None,
        }
    }

    fn collision(alias: String, name: String, span: Span) -> ComposeError {
        ComposeError::CollidingImportedNetwork { alias, name, span }
    }
}

impl ImportableDecl for Volume {
    const KIND: &'static str = "volume";

    /// `driver` joins `name` because it holds one, and only because of
    /// that: a `volume` names its driver with an ordinary literal, so
    /// there is a value to read. `driver_opts` is a map and `external` a
    /// flag, so neither can answer.
    const READABLE_FIELDS: &'static [&'static str] = &["name", "driver"];

    fn decl_name(&self) -> &str {
        &self.name.name
    }

    fn read_field(&self, field: &str) -> Option<FieldValue> {
        match field {
            "name" => Some(FieldValue::Set(self.docker_name().to_string())),
            // Unset rather than empty: a `volume` with no `driver` leaves
            // the choice to Docker, and there is no honest string for
            // "whatever Docker picks" to splice into a label.
            "driver" => Some(match &self.driver {
                Some(driver) => FieldValue::Set(driver.text().to_string()),
                None => FieldValue::Unset,
            }),
            "external" => Some(FieldValue::NotAValue(PRESENCE_FLAG)),
            "driver_opts" => Some(FieldValue::NotAValue(
                "a map of driver options rather than one value",
            )),
            _ => None,
        }
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
    mut service: Service,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    imports: &mut Imports,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<Service, ComposeError> {
    let mut acc = MergeAcc::default();
    let mut in_progress = Vec::new();

    // Ahead of the `with`-list, since the arguments in it are values
    // written in *this* scope — see `resolve_scoped_field_accesses`.
    resolve_scoped_field_accesses(&mut service.fields, scope, symbols)?;

    for inv in &service.fields.with {
        let resolved = resolve_invocation(
            inv,
            scope,
            symbols,
            cache,
            &mut in_progress,
            imports,
            warnings,
        )?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }

    let mut own = service.fields;
    own.with.clear();
    resolve_qualified_references(&mut own, scope, symbols.resolver, imports)?;
    merge_tier(&mut acc, own, &Tier::Own)?;

    let mut fields = acc.into_service_fields();
    resolve_bound_field_accesses(&mut fields, scope, symbols)?;
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
    symbols: &Symbols<'r, R>,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    in_progress: &mut Vec<(R::Scope, String)>,
    imports: &mut Imports,
    warnings: &mut Vec<ComposeWarning>,
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

    // Cloned before the `with`-list rather than after it, so that this
    // template's own field accesses — the ones in its invocation
    // arguments included — resolve against the file it was *declared*
    // in, which is `scope` here and gone by the time an argument
    // reaches the body it is substituted into.
    let mut fields = decl.fields.clone();
    resolve_scoped_field_accesses(&mut fields, scope, symbols)?;

    let mut acc = MergeAcc::default();
    for inv in &fields.with {
        let resolved =
            resolve_invocation(inv, scope, symbols, cache, in_progress, imports, warnings)?;
        merge_tier(&mut acc, resolved, &Tier::Explicit(inv.name.name.clone()))?;
    }
    let mut own = fields;
    own.with.clear();
    resolve_qualified_references(&mut own, scope, symbols.resolver, imports)?;
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
    symbols: &Symbols<'_, R>,
    cache: &mut HashMap<(R::Scope, String), ServiceFields>,
    in_progress: &mut Vec<(R::Scope, String)>,
    imports: &mut Imports,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<ServiceFields, ComposeError> {
    let (target_scope, decl) = symbols.resolver.resolve_template(
        scope,
        inv.qualifier.as_ref(),
        &inv.name.name,
        inv.span,
    )?;

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

    let mut fields = resolve_template(
        decl,
        target_scope,
        symbols,
        cache,
        in_progress,
        imports,
        warnings,
    )?;
    substitute_params(&mut fields, &args, &decl.name.name, warnings)?;
    // Whatever an argument naming an imported declaration left behind is
    // resolved here, against `scope` — the invocation's own, the only
    // one its aliases mean anything in, and one the merged fields leave
    // behind the moment they are returned (#296). A field read first,
    // since resolving one writes a plain string; then the reference
    // positions, which import the declaration exactly as a written
    // `networks [alias.name]` would; then the positions that can hold
    // neither.
    resolve_argument_field_accesses(&mut fields, scope, symbols)?;
    resolve_qualified_references(&mut fields, scope, symbols.resolver, imports)?;
    reject_qualified_values(&mut fields, &decl.name.name)?;
    Ok(fields)
}

/// Refuses a declaration reference that an argument put somewhere a
/// plain value belongs (#296) — every slot left once
/// [`resolve_argument_field_accesses`] has read the field accesses and
/// [`resolve_qualified_references`] has resolved the two reference
/// positions that can attach one.
///
/// A backstop by shape rather than by field: an argument is bound into
/// whichever slots the callee's body happens to spell `$param` in, and
/// no list of those slots kept here could stay in step with
/// [`substitute_params`]'s walk. Anything still qualified at this point
/// reached a position with no meaning for it, and reaching codegen is
/// what that used to mean.
fn reject_qualified_values(
    fields: &mut ServiceFields,
    template_name: &str,
) -> Result<(), ComposeError> {
    // Takes `&mut` because [`visit_literals_mut`] is the walk there is,
    // not because anything here is written.
    visit_literals_mut(fields, &mut |lit| {
        let Literal::Qualified(q) = lit else {
            return Ok(());
        };
        Err(ComposeError::QualifiedArgumentNotAValue {
            template: template_name.to_string(),
            alias: q.qualifier.name.clone(),
            name: q.name.clone(),
            span: q.span,
        })
    })
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
    // resolve against.
    reject_qualified(&fields.env_file, "env_file")?;
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

// ---- field access (#275) ----
//
// `proxy.name`, `traefik.proxy.name` and `$net.name` each read one field
// off a `network`/`volume` declaration in a value position, and all
// three are resolved here into a plain `Literal::Str` holding the real
// Docker name. Codegen never learns the syntax exists.
//
// The work splits across two passes, by what each stage can answer:
//
// - `resolve_scoped_field_accesses` runs on a scope's own body, in the
//   scope it was *written* in, before anything is substituted. That's
//   the only place an import alias means anything: docs/DESIGN.md's
//   lexical-scoping rule says a template's `traefik.` resolves against
//   the file the template was written in, never the file that invoked
//   it, and once the invocation is resolved that scope is gone.
// - `resolve_bound_field_accesses` runs on a service's fully merged
//   fields, once every `$param` has been substituted, and resolves what
//   was still a parameter when the first pass ran.
//
// A bare base needs neither the writing scope nor a binding — it names
// one flat namespace of the program's own declarations, the same one
// codegen resolves `networks [proxy]` against — so the first pass takes
// it as well, which is what lets an argument written `net: proxy.name`
// arrive at a `{{net}}` interpolation as ordinary text.

/// What a `network`/`volume` declaration answers when a value position
/// reads one of its fields.
///
/// The rule the three arms encode: a field is readable when it *holds a
/// value*. That is a property of the field, not a list this language
/// keeps — `name` and a `volume`'s `driver` are ordinary literals, while
/// a bare-presence flag and a nested map have nothing a string position
/// could hold. Adding a field to a declaration therefore makes it
/// readable by writing one arm in that kind's
/// [`ImportableDecl::read_field`], with no separate permission to grant.
enum FieldValue {
    /// The field's value, as the string a value position receives.
    Set(String),
    /// A field this kind has, that this declaration leaves unset.
    Unset,
    /// A field that never holds a value, and why — phrased to sit after
    /// "is" in a diagnostic sentence.
    NotAValue(&'static str),
}

/// The reason a bare-presence flag can't be read. Shared by every such
/// field so the two kinds describe `external` the same way.
const PRESENCE_FLAG: &str = "a bare-presence flag rather than a value";

/// The program's own top-level declarations, as a bare field-access
/// base sees them: one flat namespace, the same one codegen resolves
/// `networks [...]` entries and named-volume mounts against.
///
/// `services` is carried only so that `jellyfin.name` — a base that
/// names something real with no name of its own to read — gets a
/// diagnostic saying that, rather than one saying nothing declares it.
struct Declarations<'a> {
    networks: &'a [Network],
    volumes: &'a [Volume],
    services: &'a [String],
}

impl Declarations<'_> {
    /// Whether the program declares anything at all under `name` — the
    /// question [`reinterpret_argument_reference`] asks before trying a
    /// base as an import alias, so that a local declaration always wins
    /// the `IDENT "." IDENT` spelling and no field access an author
    /// already writes can change meaning. `services` counts for the
    /// reason it is carried at all: `jellyfin.name` names something
    /// real, and its own diagnostic says so.
    fn declares(&self, name: &str) -> bool {
        self.networks.iter().any(|n| n.name.name == name)
            || self.volumes.iter().any(|v| v.name.name == name)
            || self.services.iter().any(|s| s == name)
    }
}

/// Everything composition resolves a name against: `resolver` for
/// anything that crosses a `use` boundary, `decls` for the program's
/// own declarations.
///
/// One value rather than two parameters because
/// [`compose_service`]/[`resolve_template`]/[`resolve_invocation`]
/// already thread seven of those through their mutual recursion, and
/// these two are only ever wanted together.
struct Symbols<'a, R: SymbolResolver> {
    resolver: &'a R,
    decls: Declarations<'a>,
}

/// Resolves one field access to the text it stands for, or `None` when
/// it can't be resolved *yet* — a base still naming a template
/// parameter, which only [`resolve_scoped_field_accesses`] ever sees and
/// which substitution makes concrete before the second pass runs.
fn resolve_field_access<R: SymbolResolver>(
    access: &FieldAccess,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
) -> Result<Option<String>, ComposeError> {
    match &access.base {
        Literal::Param(_, _) => Ok(None),
        // An alias-qualified base is asked of both kinds:
        // `traefik.proxy.name` says nothing about which one `proxy` is,
        // and both answer the same field. Matching on the resolver's own
        // "no such network"/"no such volume" is what tells "this scope
        // declares neither" apart from "the alias itself doesn't
        // resolve" — the second is a different mistake with a different
        // fix, so it's reported as it stands rather than folded in.
        Literal::Qualified(q) => {
            match symbols
                .resolver
                .resolve_qualified_network(scope, &q.qualifier, &q.name, q.span)
            {
                Ok(decl) => declaration_field(decl, &access.field).map(Some),
                Err(ComposeError::UnknownQualifiedNetwork { .. }) => match symbols
                    .resolver
                    .resolve_qualified_volume(scope, &q.qualifier, &q.name, q.span)
                {
                    Ok(decl) => declaration_field(decl, &access.field).map(Some),
                    Err(ComposeError::UnknownQualifiedVolume { .. }) => {
                        Err(ComposeError::UnknownQualifiedDeclaration {
                            alias: q.qualifier.name.clone(),
                            name: q.name.clone(),
                            span: access.span,
                        })
                    }
                    Err(other) => Err(other),
                },
                Err(other) => Err(other),
            }
        }
        // Everything else resolves by its own text: a bare identifier as
        // written, and a quoted string for an invocation that bound the
        // parameter to one (`with caddy { net: "proxy" }`) — which names
        // a declaration exactly as the bare spelling does in every other
        // position that takes a reference.
        base => {
            let name = base.text();
            if let Some(decl) = symbols.decls.networks.iter().find(|n| n.name.name == name) {
                return declaration_field(decl, &access.field).map(Some);
            }
            if let Some(decl) = symbols.decls.volumes.iter().find(|v| v.name.name == name) {
                return declaration_field(decl, &access.field).map(Some);
            }
            if symbols.decls.services.iter().any(|s| s == name) {
                return Err(ComposeError::FieldBaseNotDeclaration {
                    base: name.to_string(),
                    found: "a service",
                    span: access.span,
                });
            }
            Err(ComposeError::FieldBaseNotDeclared {
                base: name.to_string(),
                span: access.span,
            })
        }
    }
}

/// Reads one field off a resolved declaration, or names the fields it
/// hasn't got. The span reported is the *field*'s own rather than the
/// whole access's: the base resolved fine, so the field is the half to
/// edit.
fn declaration_field<D: ImportableDecl>(decl: &D, field: &Ident) -> Result<String, ComposeError> {
    match decl.read_field(&field.name) {
        Some(FieldValue::Set(value)) => Ok(value),
        Some(FieldValue::Unset) => Err(ComposeError::DeclarationFieldUnset {
            kind: D::KIND,
            decl: decl.decl_name().to_string(),
            field: field.name.clone(),
            span: field.span,
        }),
        Some(FieldValue::NotAValue(what)) => Err(ComposeError::DeclarationFieldNotAValue {
            kind: D::KIND,
            decl: decl.decl_name().to_string(),
            field: field.name.clone(),
            what,
            span: field.span,
        }),
        None => Err(ComposeError::UnknownDeclarationField {
            kind: D::KIND,
            decl: decl.decl_name().to_string(),
            field: field.name.clone(),
            readable: D::READABLE_FIELDS,
            span: field.span,
        }),
    }
}

/// Renders a kind's readable fields the way a sentence wants them:
/// ``\`name\``, or ``\`name\` and \`driver\``. Kept beside
/// [`declaration_field`] because it exists only for that diagnostic.
fn readable_field_list(fields: &[&str]) -> String {
    let quoted: Vec<String> = fields.iter().map(|f| format!("`{f}`")).collect();
    match quoted.as_slice() {
        [] => "no fields".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Reads a dotted `{{binding}}` as the field access it spells, or
/// `None` for a binding with no `.` in it — `{{name}}` and every other
/// undotted binding mean what they always meant, and belong to whichever
/// stage answers them.
///
/// Every span in the result is the enclosing string literal's: a binding
/// is string *content*, with no tokens of its own to draw a narrower one
/// from. Arity reads exactly as it does in source — see
/// `parser::Parser::parse_field_access` — since two spellings of one
/// thing that disagreed about what `a.b.c` means would be a trap rather
/// than a convenience.
fn binding_field_access(binding: &str, span: Span) -> Result<Option<FieldAccess>, ComposeError> {
    let segments: Vec<&str> = binding.split('.').collect();
    let ident = |name: &str| Ident {
        name: name.to_string(),
        span,
    };
    let (base, field) = match segments.as_slice() {
        [_] => return Ok(None),
        [base, field] => (Literal::Ident((*base).to_string(), span), *field),
        [alias, name, field] => (
            Literal::Qualified(Box::new(QualifiedRef {
                qualifier: ident(alias),
                name: (*name).to_string(),
                name_span: span,
                span,
            })),
            *field,
        ),
        _ => {
            return Err(ComposeError::InterpolatedFieldAccessTooDeep {
                text: binding.to_string(),
                span,
            });
        }
    };
    Ok(Some(FieldAccess {
        base,
        field: ident(field),
        span,
    }))
}

/// Resolves every field access a scope can answer on its own, over that
/// scope's own written body: an alias-qualified base, which only means
/// something here, and a bare one, which means the same thing
/// everywhere.
///
/// Runs before the body's `with`-list is resolved, and therefore over
/// the invocation arguments too — an argument is written at the call
/// site, so `with inner { n: traefik.proxy.name }` inside a template
/// must resolve `traefik` against that template's own file, and by the
/// time the argument reaches `inner`'s body the file it came from is no
/// longer in hand. That's why this isn't folded into
/// [`resolve_qualified_references`], which runs one step later, on a
/// body whose `with`-list has already been cleared.
///
/// A `$param` base is left for [`resolve_bound_field_accesses`], and so
/// is a two-segment `{{binding}}`: at this point `{{net.name}}` can't be
/// told apart from a declaration called `net`, since a parameter is
/// spelled without its sigil inside string content. Only the
/// three-segment `{{alias.decl.field}}` spelling is claimed here, and it
/// can't collide with a parameter — a parameter names one declaration
/// already, so nothing legal follows its own field.
///
/// The `with`-list's arguments get [`reinterpret_argument_reference`]
/// first, since a two-segment `alias.decl` there is a declaration being
/// passed rather than a field being read (#296).
fn resolve_scoped_field_accesses<R: SymbolResolver>(
    fields: &mut ServiceFields,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
) -> Result<(), ComposeError> {
    for inv in &mut fields.with {
        for entry in &mut inv.args.entries {
            visit_raw_literals_mut(&mut entry.value, &mut |lit| {
                reinterpret_argument_reference(lit, scope, symbols)
            })?;
        }
    }
    visit_literals_mut(fields, &mut |lit| {
        resolve_literal_field_access(lit, scope, symbols, AccessScope::Written)
    })
}

/// Reads a two-segment `alias.decl` in a `with`-invocation argument as
/// the imported declaration it names, rewriting the
/// [`Literal::Field`] the value grammar parsed into the
/// [`Literal::Qualified`] reference it actually is (#296).
///
/// An argument is the one value position where naming a *declaration*
/// means something: a parameter bound to one reaches `networks [$net]`
/// and `{{net.name}}` inside the callee, exactly as a bare same-file
/// `IDENT` argument already does. Every other value position has only
/// the field reading to offer, which is why
/// [`crate::parser::Parser::parse_field_access`]'s arity rule — two
/// segments are a local declaration and a field — still decides
/// everywhere else, and still decides *here* whenever the base names a
/// local declaration. Only a base that names none of the program's own
/// declarations is tried as an alias, so no `alias.decl` reading can
/// take an existing field access away.
///
/// The rewrite happens in the scope the argument was *written* in, and
/// that is the whole point: `resolve_invocation` resolves what it
/// leaves behind against that same scope, before the argument travels
/// into a callee whose own file knows nothing of the alias.
///
/// A base that isn't an alias either is left exactly as it was, for
/// `resolve_field_access` to report as the field access it was parsed
/// as. A base that *is* an alias holding no such declaration is the one
/// case reported from here: the alias resolved, so
/// "no such local declaration" would name the wrong mistake.
fn reinterpret_argument_reference<R: SymbolResolver>(
    lit: &mut Literal,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
) -> Result<(), ComposeError> {
    let Literal::Field(access) = lit else {
        return Ok(());
    };
    let Literal::Ident(base, base_span) = &access.base else {
        return Ok(());
    };
    if symbols.decls.declares(base) {
        return Ok(());
    }
    let qualifier = Ident {
        name: base.clone(),
        span: *base_span,
    };
    let name = &access.field.name;
    // Both kinds are asked, and in the same order `resolve_field_access`
    // asks them: `shared.proxy` says nothing about whether `proxy` is a
    // network or a volume, and either can be passed.
    match symbols
        .resolver
        .resolve_qualified_network(scope, &qualifier, name, access.span)
    {
        Ok(_) => {}
        Err(ComposeError::UnknownQualifiedNetwork { .. }) => match symbols
            .resolver
            .resolve_qualified_volume(scope, &qualifier, name, access.span)
        {
            Ok(_) => {}
            Err(ComposeError::UnknownQualifiedVolume { .. }) => {
                return Err(ComposeError::UnknownQualifiedDeclaration {
                    alias: qualifier.name,
                    name: name.clone(),
                    span: access.span,
                });
            }
            Err(other) => return Err(other),
        },
        // `UnknownAlias`, most of all: the base names no import either,
        // so this was a field access all along and stays one.
        Err(_) => return Ok(()),
    }
    *lit = Literal::Qualified(Box::new(QualifiedRef {
        name: name.clone(),
        name_span: access.field.span,
        span: access.span,
        qualifier,
    }));
    Ok(())
}

/// Resolves every field access left once substitution has bound each
/// `$param` to a concrete value, over a service's fully merged fields —
/// `$net.name`, and the `{{net.name}}` interpolation substitution
/// rewrote to `{{proxy.name}}` on its way here.
///
/// Runs beside [`check_numeric_fields`], and before it: both ask what a
/// finished service holds, and a field access has to be the string it
/// resolves to before anything judges the literal sitting in a field.
fn resolve_bound_field_accesses<R: SymbolResolver>(
    fields: &mut ServiceFields,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
) -> Result<(), ComposeError> {
    visit_literals_mut(fields, &mut |lit| {
        resolve_literal_field_access(lit, scope, symbols, AccessScope::Bound)
    })
}

/// Resolves the alias-qualified field accesses one invocation's
/// arguments left in the callee's substituted body — `$net.name` and
/// `{{net.name}}` with `net` bound to an imported declaration, which
/// substitution turned into `alias.decl.field` (#296).
///
/// Runs in the scope the *invocation* was written in, which is the only
/// scope the alias means anything in, and the last moment it is still in
/// hand: the merged fields travel on to a service whose own file need
/// never have imported it. Everything else is left exactly where the two
/// existing passes already put it — a same-file argument's
/// `{{proxy.name}}` still resolves once the service's fields are merged,
/// so an access in a contribution the service's own body overrides goes
/// on drawing no diagnostic.
fn resolve_argument_field_accesses<R: SymbolResolver>(
    fields: &mut ServiceFields,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
) -> Result<(), ComposeError> {
    visit_literals_mut(fields, &mut |lit| {
        resolve_literal_field_access(lit, scope, symbols, AccessScope::Substituted)
    })
}

/// Which accesses a pass claims, since neither a binding's head nor a
/// field access's base carries a sigil saying whether it names a
/// parameter, a local declaration, or an import alias.
#[derive(Clone, Copy, PartialEq)]
enum AccessScope {
    /// A scope's own written body: every whole-slot field access (a
    /// `$param` base resolving to `None` until it is bound), and only
    /// the `{{alias.decl.field}}` binding, whose head can't be a
    /// parameter.
    Written,
    /// An invocation's substituted body: only what an *argument* put
    /// there, which is alias-qualified in both spellings. A base or head
    /// that is anything else belongs to a later pass — leaving it alone
    /// keeps this addition from moving any existing diagnostic earlier.
    Substituted,
    /// A finished service: every access left, parameters having been
    /// substituted away.
    Bound,
}

impl AccessScope {
    /// Whether a whole-slot field access with this `base` is this pass's
    /// to resolve.
    fn claims(self, base: &Literal) -> bool {
        match self {
            AccessScope::Written | AccessScope::Bound => true,
            AccessScope::Substituted => matches!(base, Literal::Qualified(_)),
        }
    }

    /// Whether a dotted `{{binding}}` reading as this `base` is this
    /// pass's to resolve. Stricter than [`Self::claims`] for
    /// [`Self::Written`] alone: a two-segment binding there can't be
    /// told from a parameter, which is spelled without its sigil inside
    /// string content.
    fn claims_binding(self, base: &Literal) -> bool {
        match self {
            AccessScope::Bound => true,
            AccessScope::Written | AccessScope::Substituted => {
                matches!(base, Literal::Qualified(_))
            }
        }
    }
}

/// Resolves whatever field access one literal slot holds: the slot
/// itself, when it *is* a field access, and any dotted binding inside a
/// string's content.
///
/// A resolved access becomes a [`Literal::Str`] carrying the whole
/// access's span, so a later diagnostic about the value points at where
/// the access was written rather than at the declaration it read.
fn resolve_literal_field_access<R: SymbolResolver>(
    lit: &mut Literal,
    scope: R::Scope,
    symbols: &Symbols<'_, R>,
    pass: AccessScope,
) -> Result<(), ComposeError> {
    match lit {
        Literal::Field(access) => {
            debug_assert!(
                pass != AccessScope::Bound || !matches!(access.base, Literal::Param(_, _)),
                "substitution binds every parameter base before the second pass runs"
            );
            if !pass.claims(&access.base) {
                return Ok(());
            }
            if let Some(resolved) = resolve_field_access(access, scope, symbols)? {
                *lit = Literal::Str(resolved, access.span);
            }
            Ok(())
        }
        Literal::Str(text, span) => {
            let span = *span;
            let resolved = interp::resolve_with(text, |binding| {
                let Some(access) = binding_field_access(binding, span)? else {
                    return Ok(None);
                };
                if !pass.claims_binding(&access.base) {
                    return Ok(None);
                }
                resolve_field_access(&access, scope, symbols)
            })?;
            *text = resolved;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Hands `visit` every [`Literal`] slot in `fields`, in source-ish
/// order, recursing into the [`RawValue`] trees a `raw` body and a
/// `with`-invocation's arguments hold.
///
/// The same set of slots [`substitute_params`] walks, and the same
/// reason to be exhaustive: a slot missing from that walk leaves a
/// `$param` for codegen to emit as its own name (#168's bug class), and
/// a slot missing from this one leaves a `Literal::Field` for codegen to
/// emit as the field's name. The two stay separate walks because
/// `substitute_params` treats its slots *differently* — reference-shaped,
/// `number`-typed, or plain — while every slot here answers the same
/// question, and threading that distinction through a shared walk would
/// buy uniformity at the price of a callback that has to re-derive it.
/// Adding a literal-carrying field to [`ServiceFields`] means adding it
/// to both.
fn visit_literals_mut(
    fields: &mut ServiceFields,
    visit: &mut impl FnMut(&mut Literal) -> Result<(), ComposeError>,
) -> Result<(), ComposeError> {
    if let Some(img) = &mut fields.image
        && let Some(r) = &mut img.reference
    {
        visit(r)?;
    }
    if let Some(b) = &mut fields.build {
        for lit in [b.context.as_mut(), b.dockerfile.as_mut()]
            .into_iter()
            .flatten()
        {
            visit(lit)?;
        }
    }
    if let Some(e) = &mut fields.expose
        && let Some(p) = &mut e.port
    {
        visit(p)?;
    }
    if let Some(r) = &mut fields.restart
        && let Some(p) = &mut r.policy
    {
        visit(p)?;
    }
    if let Some(cn) = &mut fields.container_name {
        visit(cn)?;
    }
    match &mut fields.command {
        Some(Command::Shell(lit)) => visit(lit)?,
        Some(Command::Exec(items, _)) => {
            for item in items {
                visit(item)?;
            }
        }
        None => {}
    }
    match &mut fields.entrypoint {
        Some(Entrypoint::Shell(lit)) => visit(lit)?,
        Some(Entrypoint::Exec(items, _)) => {
            for item in items {
                visit(item)?;
            }
        }
        None => {}
    }
    if let Some(hc) = &mut fields.healthcheck {
        match &mut hc.test {
            Some(HealthcheckTest::Shell(lit)) => visit(lit)?,
            Some(HealthcheckTest::Exec(items, _)) => {
                for item in items {
                    visit(item)?;
                }
            }
            None => {}
        }
        for lit in [
            hc.interval.as_mut(),
            hc.timeout.as_mut(),
            hc.retries.as_mut(),
            hc.start_period.as_mut(),
            hc.start_interval.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            visit(lit)?;
        }
    }
    for entries in [
        &mut fields.volumes.entries,
        &mut fields.publish.entries,
        &mut fields.devices.entries,
    ] {
        for entry in entries.iter_mut() {
            // A named-volume host resolves to a `volume` declaration, so
            // it's a reference rather than a value and can hold no field
            // access — but it's a `Literal` slot like any other, and
            // visiting it costs nothing beyond one `match` that finds
            // neither a field access nor a string to scan.
            match &mut entry.host {
                ArrowMapHost::BindMount(host) | ArrowMapHost::Named(host) => visit(host)?,
            }
            visit(&mut entry.container)?;
        }
    }
    for e in &mut fields.env.entries {
        visit(&mut e.key)?;
        visit(&mut e.value)?;
    }
    for e in &mut fields.labels.entries {
        visit(&mut e.key)?;
        // A label value may be a list since #288, so this walks whatever
        // it holds rather than the one literal it used to be.
        for lit in e.value.literals_mut() {
            visit(lit)?;
        }
    }
    for entry in &mut fields.raw.entries {
        visit(&mut entry.key)?;
        visit_raw_literals_mut(&mut entry.value, visit)?;
    }
    // A `with`-invocation's arguments are values written at the call
    // site, so they carry field accesses like any other value — and
    // resolving them where they were written is the whole reason
    // `resolve_scoped_field_accesses` runs before the `with`-list does.
    for inv in &mut fields.with {
        for entry in &mut inv.args.entries {
            visit(&mut entry.key)?;
            visit_raw_literals_mut(&mut entry.value, visit)?;
        }
    }
    for lit in fields
        .networks
        .iter_mut()
        .chain(&mut fields.dns)
        .chain(&mut fields.env_file)
    {
        visit(lit)?;
    }
    for entry in &mut fields.depends_on {
        visit(&mut entry.reference)?;
    }
    Ok(())
}

/// [`visit_literals_mut`]'s recursion into one schema-free
/// [`RawValue`] tree — a `raw` entry's value, or a `with`-invocation
/// argument. Both halves of a nested map are visited, keys included,
/// for the reason [`substitute_params`] gives at the same spot: codegen
/// resolves interpolation on both sides of every `raw` entry it emits,
/// so anything less would leave the two stages disagreeing about which
/// halves a value may be written into.
fn visit_raw_literals_mut(
    value: &mut RawValue,
    visit: &mut impl FnMut(&mut Literal) -> Result<(), ComposeError>,
) -> Result<(), ComposeError> {
    match value {
        RawValue::Literal(lit) => visit(lit)?,
        RawValue::List(items, _) => {
            for item in items {
                visit_raw_literals_mut(item, visit)?;
            }
        }
        RawValue::Map(entries, _) => {
            for (key, v) in entries {
                visit(key)?;
                visit_raw_literals_mut(v, visit)?;
            }
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
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    if let Some(img) = &mut fields.image
        && let Some(r) = &mut img.reference
    {
        substitute_literal(r, args, template_name, warnings)?;
    }
    // `build`'s own literal slots (#224) — both plain free-text paths,
    // so both take an ordinary `substitute_literal`. Missing either
    // would reproduce #168's live bug class: the `Literal::Param`
    // survives to codegen, which writes the parameter's own name into
    // the generated `build:` key and exits 0.
    if let Some(b) = &mut fields.build {
        if let Some(c) = &mut b.context {
            substitute_literal(c, args, template_name, warnings)?;
        }
        if let Some(d) = &mut b.dockerfile {
            substitute_literal(d, args, template_name, warnings)?;
        }
    }
    if let Some(e) = &mut fields.expose
        && let Some(p) = &mut e.port
    {
        substitute_numeric_literal(p, args, template_name, warnings)?;
    }
    if let Some(r) = &mut fields.restart
        && let Some(p) = &mut r.policy
    {
        substitute_literal(p, args, template_name, warnings)?;
    }
    if let Some(cn) = &mut fields.container_name {
        substitute_literal(cn, args, template_name, warnings)?;
    }
    // `command`'s literals (#156) go through the same substitution walk
    // as every other `Literal` slot above, so a `$param` reference
    // inside a `command ["--user=$user"]` entry gets resolved here — see
    // `ast::Literal::Param`'s own doc for why a `Param` surviving this
    // pass unresolved would be a bug.
    match &mut fields.command {
        Some(Command::Shell(lit)) => substitute_literal(lit, args, template_name, warnings)?,
        Some(Command::Exec(items, _)) => {
            for item in items {
                substitute_literal(item, args, template_name, warnings)?;
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
        Some(Entrypoint::Shell(lit)) => substitute_literal(lit, args, template_name, warnings)?,
        Some(Entrypoint::Exec(items, _)) => {
            for item in items {
                substitute_literal(item, args, template_name, warnings)?;
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
            Some(HealthcheckTest::Shell(lit)) => {
                substitute_literal(lit, args, template_name, warnings)?
            }
            Some(HealthcheckTest::Exec(items, _)) => {
                for item in items {
                    substitute_literal(item, args, template_name, warnings)?;
                }
            }
            None => {}
        }
        // `retries` is `book/src/built-in-fields.md`'s other `number`-typed
        // field alongside `expose.port`, so it takes the numeric-checked
        // substitution rather than riding the loop below with its four
        // string-typed siblings.
        if let Some(retries) = &mut hc.retries {
            substitute_numeric_literal(retries, args, template_name, warnings)?;
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
            substitute_literal(lit, args, template_name, warnings)?;
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
                    substitute_literal(host, args, template_name, warnings)?;
                }
                ArrowMapHost::Named(_) => debug_assert_eq!(
                    field_name, "volume",
                    "only `volume` sets key_may_be_reference, so only its \
                     entries can carry a named host"
                ),
            }
            substitute_literal(&mut entry.container, args, template_name, warnings)?;
        }
    }
    for e in &mut fields.env.entries {
        substitute_literal(&mut e.key, args, template_name, warnings)?;
        substitute_literal(&mut e.value, args, template_name, warnings)?;
    }
    // `labels` (#243) substitutes exactly like `env` just above: both
    // sides are plain `Literal` slots, so a template may parameterize
    // either a label's key or its value.
    for e in &mut fields.labels.entries {
        substitute_literal(&mut e.key, args, template_name, warnings)?;
        for lit in e.value.literals_mut() {
            substitute_literal(lit, args, template_name, warnings)?;
        }
    }
    for entry in &mut fields.raw.entries {
        substitute_literal(&mut entry.key, args, template_name, warnings)?;
        substitute_raw_value(&mut entry.value, args, template_name, warnings)?;
    }
    for inv in &mut fields.with {
        for entry in &mut inv.args.entries {
            substitute_literal(&mut entry.key, args, template_name, warnings)?;
            substitute_raw_value(&mut entry.value, args, template_name, warnings)?;
        }
    }
    // The reference-shaped list fields #196 newly opened to `$param` —
    // `networks`, `dns`, `env_file`, and a `depends_on`
    // entry's own reference — walk through
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
    substitute_reference_list(&mut fields.networks, args, template_name, warnings)?;
    substitute_reference_list(&mut fields.dns, args, template_name, warnings)?;
    substitute_reference_list(&mut fields.env_file, args, template_name, warnings)?;
    // `depends_on` splices like the three above (#283), but an entry
    // carries a `condition` as well as a name, so an expanded item
    // inherits the condition written on the parameter's own entry:
    // `depends_on [$deps { condition: service_healthy }]` means every
    // service `deps` names, healthy. Cloning it is the only reading that
    // doesn't silently drop what the author wrote.
    let mut depends_on = Vec::with_capacity(fields.depends_on.len());
    for mut entry in std::mem::take(&mut fields.depends_on) {
        let bound = match &entry.reference {
            Literal::Param(name, _) => match args.get(name.as_str()) {
                Some(RawValue::List(items, _)) => Some((name.clone(), items)),
                _ => None,
            },
            _ => None,
        };
        let Some((param, items)) = bound else {
            substitute_reference_literal(&mut entry.reference, args, template_name, warnings)?;
            depends_on.push(entry);
            continue;
        };
        for item in items {
            depends_on.push(DependsOnEntry {
                reference: reference_list_item(item, &param, template_name)?.clone(),
                condition: entry.condition,
                span: entry.span,
            });
        }
    }
    fields.depends_on = depends_on;
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
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    let Some(name) = param_name else {
        // Not a whole-slot `$param`, so this is where the *other* two
        // ways a parameter reaches a value get their turn: `{{param}}`
        // inside string content (#266), and a `$param.name` field access
        // whose base is what the argument binds (#275). Only a `Str` can
        // hold the first — an `Ident` can't contain `{`, and a `Number`
        // can't contain anything but digits.
        match lit {
            Literal::Str(text, span) => {
                let span = *span;
                let resolved =
                    substitute_string_content(text, span, args, template_name, warnings)?;
                *text = resolved;
            }
            Literal::Field(access) => substitute_field_base(access, args, template_name)?,
            _ => {}
        }
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

/// Binds a `$param.name` field access's base to the invocation's
/// argument, leaving the access itself for
/// [`resolve_bound_field_accesses`] to read once the base names
/// something concrete (#275).
///
/// Only a name can carry a field, so anything else the caller passed is
/// rejected here rather than left to resolve into nothing later —
/// reported at the argument's own call site, exactly as
/// [`substitute_reference_literal`] and [`substitute_numeric_literal`]
/// report theirs, and for the same reason: the argument is the half
/// that has to change, and one template body is reached from many call
/// sites.
///
/// A forwarded parameter (`with inner { n: $net }`) replaces one
/// [`Literal::Param`] base with another, so the access resolves at
/// whichever call site finally binds a real declaration — the same
/// deferral a whole-slot `$param` already gets.
fn substitute_field_base(
    access: &mut FieldAccess,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
) -> Result<(), ComposeError> {
    let Literal::Param(param, _) = &access.base else {
        return Ok(());
    };
    let param = param.clone();
    let replacement = args
        .get(param.as_str())
        .expect("param name was already validated against the template's declared params");
    match replacement {
        RawValue::Literal(
            lit @ (Literal::Ident(_, _)
            | Literal::Str(_, _)
            | Literal::Param(_, _)
            | Literal::Qualified(_)),
        ) => {
            access.base = lit.clone();
            Ok(())
        }
        other => Err(ComposeError::ArgumentCantCarryField {
            template: template_name.to_string(),
            param,
            found: argument_kind(other),
            span: other.span(),
        }),
    }
}

/// The one `{{binding}}` name composition never resolves: `{{name}}` is
/// the *enclosing service's* own name, and only codegen knows which
/// service a template's contribution finally landed on. Reserving it
/// here is what keeps #266 additive — every `{{name}}` written before a
/// parameter could be interpolated at all still means what it meant
/// then, whatever a template happens to call its parameters. A template
/// that declares a parameter by this name reaches it as `$name` and gets
/// [`ComposeWarning::NameParameterNotInterpolated`] if its body also
/// interpolates the binding.
const SERVICE_NAME_BINDING: &str = "name";

/// Resolves `{{param}}` interpolation inside one string literal's
/// content against the invocation's bound arguments, and reports the two
/// spellings that look like they do this and don't (see
/// [`ComposeWarning`]).
///
/// This runs at composition rather than at codegen for the reason
/// [`crate::interp`]'s own doc gives: an invocation's arguments exist
/// only while the invocation is being resolved. What it leaves behind —
/// `{{name}}`, and any binding this template has no parameter for — is
/// passed through untouched for codegen's `interp::resolve` to either
/// resolve or reject, so a genuine typo still reports as an unknown
/// interpolation rather than reaching the generated YAML.
fn substitute_string_content(
    text: &str,
    span: Span,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<String, ComposeError> {
    for param in inert_param_refs(text, args) {
        warn(
            warnings,
            ComposeWarning::InertParameterInString {
                template: template_name.to_string(),
                param: param.to_string(),
                span,
            },
        );
    }
    interp::resolve_with(text, |binding| {
        if binding == SERVICE_NAME_BINDING {
            if args.contains_key(SERVICE_NAME_BINDING) {
                warn(
                    warnings,
                    ComposeWarning::NameParameterNotInterpolated {
                        template: template_name.to_string(),
                        span,
                    },
                );
            }
            return Ok(None);
        }
        // A dotted binding is an interpolated field access (#275), and
        // what an argument binds is its *head* — `{{net.name}}` reads
        // `name` off whatever `net` names, not off a parameter called
        // `net.name`. So this rewrites the head and hands the shortened
        // access on: `{{proxy.name}}` for a concrete argument, resolved
        // once the service's fields are merged, or `{{outer.name}}` for
        // a forwarded parameter, which is exactly the rename an
        // undotted forwarded `{{h}}` already gets from
        // [`interpolated_text`].
        if let Some((head, field)) = binding.split_once('.') {
            let Some(arg) = args.get(head) else {
                return Ok(None);
            };
            let Some(base) = field_base_text(arg) else {
                return Err(ComposeError::ArgumentCantCarryField {
                    template: template_name.to_string(),
                    param: head.to_string(),
                    found: argument_kind(arg),
                    span: arg.span(),
                });
            };
            return Ok(Some(format!("{{{{{base}.{field}}}}}")));
        }
        let Some(arg) = args.get(binding) else {
            return Ok(None);
        };
        // The `Err` names the value that couldn't answer, which is the
        // argument itself for a plain one and the offending *item* for a
        // list — so a bad item is reported where it is written rather
        // than at the list that holds it.
        match interpolated_text(arg) {
            Ok(text) => Ok(Some(text)),
            Err(bad) => Err(ComposeError::ArgumentNotInterpolable {
                template: template_name.to_string(),
                param: binding.to_string(),
                found: argument_kind(bad),
                span: bad.span(),
            }),
        }
    })
}

/// The text one bound argument contributes to the string it is
/// interpolated into — `None` for an argument with no honest text form,
/// which [`substitute_string_content`] turns into
/// [`ComposeError::ArgumentNotInterpolable`].
///
/// A [`Literal::Param`] argument is the interesting case: a template
/// forwarding its own parameter into a nested invocation (`template
/// outer(host) { with inner { h: $host } }`) has nothing concrete to
/// splice yet, since `outer`'s `host` isn't bound until `outer` itself
/// is invoked. Rather than failing, the interpolation is *renamed* into
/// the enclosing template's parameter namespace — `inner`'s `{{h}}`
/// becomes `{{host}}` — which is exactly the scope the string now lives
/// in, so the substitution that eventually binds `outer` resolves it.
/// This mirrors what [`substitute_literal`] already does for a
/// whole-slot `$param`, where forwarding replaces one
/// [`Literal::Param`] with another.
///
/// The one corner that leaves: forwarding a parameter *named* `name`
/// renames the interpolation to `{{name}}`, which
/// [`SERVICE_NAME_BINDING`] then hands to codegen as the service name.
/// That collision is what [`ComposeWarning::NameParameterNotInterpolated`]
/// exists to surface at the declaration that causes it.
fn interpolated_text(arg: &RawValue) -> Result<String, &RawValue> {
    let RawValue::List(items, _) = arg else {
        return scalar_interpolated_text(arg);
    };
    // A list contributes its items, comma-joined (#283), each answering
    // through [`scalar_interpolated_text`] — so a list of strings, of
    // numbers, of bare identifiers, of forwarded parameters or of field
    // accesses all work, and an item with no text form is refused as
    // itself rather than as the list around it, putting the diagnostic
    // on the item the author has to change.
    //
    // Items go through the *scalar* function rather than recursing here,
    // which is what refuses a nested list instead of flattening it. A
    // flatten would be the silent coercion
    // [`ComposeError::TemplateArgumentNotScalar`] exists to refuse: this
    // is a transpiler, and `[a, [b]]` and `[a, b]` are different values
    // that would otherwise render alike.
    //
    // The comma is the whole join rule, with no way to ask for another
    // separator. It's what both places the built-ins join use
    // (`entrypoints`, `middlewares`), and a second spelling would be
    // interpolation syntax to design and teach for a case no caller has
    // yet had.
    //
    // An empty list joins to the empty string rather than drawing an
    // error. The join of nothing is nothing, which is the honest answer,
    // and the reason to refuse it would be that an empty string makes a
    // bare `key=` label — a judgement about a use this function can't
    // see, and one #270 has already settled the other way.
    // `command "run {{args}}"` with no args is the same interpolation
    // and plainly right.
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&scalar_interpolated_text(item)?);
    }
    Ok(out)
}

/// The text one *item* of an interpolated value contributes — every
/// [`interpolated_text`] case except the list join itself, which is what
/// keeps a nested list an error rather than a flatten.
fn scalar_interpolated_text(arg: &RawValue) -> Result<String, &RawValue> {
    match arg {
        RawValue::Literal(Literal::Param(name, _)) => Ok(format!("{{{{{name}}}}}")),
        RawValue::Literal(Literal::Str(text, _)) => Ok(text.clone()),
        RawValue::Literal(Literal::Number { text, .. }) => Ok(text.clone()),
        RawValue::Literal(Literal::Ident(name, _)) => Ok(name.clone()),
        // A field-access argument defers exactly the way a forwarded
        // parameter does, as the dotted binding it already spells
        // (#275): `{{p}}` bound to `proxy.name` becomes
        // `{{proxy.name}}`, which `resolve_bound_field_accesses` reads
        // once the service's fields are merged. Reading the declaration
        // here instead would resolve it in whichever scope this
        // interpolation happens to be resolved in, rather than the one
        // the argument was written in.
        RawValue::Literal(Literal::Field(access)) => Ok(format!("{{{{{}}}}}", access.dotted())),
        // An imported declaration contributes the bare name it is
        // reached by (#296), which is the same text a same-file
        // declaration's own identifier contributes one arm up — and the
        // same text `resolve_qualified_references` rewrites the
        // reference to, so `networks [$net]` and `"{{net}}"` in one
        // template body go on naming one network.
        RawValue::Literal(Literal::Qualified(q)) => Ok(q.name.clone()),
        RawValue::List(_, _) | RawValue::Map(_, _) => Err(arg),
    }
}

/// The text one bound argument contributes as a field-access *base* —
/// `None` for an argument that can't name a declaration at all, which
/// [`substitute_string_content`] and [`substitute_field_base`] both turn
/// into [`ComposeError::ArgumentCantCarryField`].
///
/// A [`Literal::Param`] answers with its own name, sigil-free, because
/// what the caller builds from this is a `{{binding}}`: forwarding `net`
/// into a nested invocation leaves `{{net.name}}` for the call site that
/// finally binds `net`, which reads the head exactly as this one did.
fn field_base_text(arg: &RawValue) -> Option<String> {
    match arg {
        RawValue::Literal(
            Literal::Ident(name, _) | Literal::Str(name, _) | Literal::Param(name, _),
        ) => Some(name.clone()),
        // An imported declaration answers with both its segments (#296),
        // so `{{net.name}}` becomes the three-segment
        // `{{alias.decl.name}}` that says which file to resolve it in —
        // which `resolve_argument_field_accesses` then does, at the
        // invocation, while that file is still in hand.
        RawValue::Literal(Literal::Qualified(q)) => {
            Some(format!("{}.{}", q.qualifier.name, q.name))
        }
        _ => None,
    }
}

/// What one bound argument *is*, in the vocabulary
/// [`numeric_mismatch`] already uses for its own mismatches — the
/// `found` half of every diagnostic that has to say why an argument
/// couldn't be used where it was: [`ComposeError::ArgumentNotInterpolable`]
/// (no text form to splice) and [`ComposeError::ArgumentCantCarryField`]
/// (nothing a field could be read off).
///
/// Total rather than scoped to the kinds one of those refuses, so that
/// neither has to be re-taught the vocabulary when the other's set of
/// refused kinds changes. The qualified arm stays a backstop for both:
/// an argument *can* name an imported declaration since #296, but that
/// is precisely a kind both of these accept — it interpolates as its
/// bare name and it carries a field — so neither diagnostic reaches the
/// arm today. [`numeric_mismatch`]'s own qualified arm is the one a
/// declaration argument does reach, in a `number`-typed field.
fn argument_kind(arg: &RawValue) -> &'static str {
    match arg {
        RawValue::Literal(Literal::Str(_, _)) => "a quoted string",
        RawValue::Literal(Literal::Number { .. }) => "a number",
        RawValue::Literal(Literal::Ident(_, _)) => "a bare identifier",
        RawValue::Literal(Literal::Param(_, _)) => "a parameter",
        RawValue::Literal(Literal::Field(_)) => "a field access",
        RawValue::Literal(Literal::Qualified(_)) => "a qualified reference",
        RawValue::List(_, _) => "a list",
        RawValue::Map(_, _) => "a nested map",
    }
}

/// Every `$param` written inside string content that names one of
/// `args`' parameters — the spelling
/// [`ComposeWarning::InertParameterInString`] is about — in the order
/// they appear, so a string holding two of them warns about them
/// left to right rather than in `HashMap` order.
///
/// Scanned out of the text rather than by searching the text for each
/// parameter's name, which would match `$host` inside `$hostname`. The
/// run taken after the `$` is exactly what the lexer's own `scan_ident`
/// would take, so `$user-agent` names the parameter `user-agent` or
/// nothing at all — never `user`.
///
/// `${ident}` is deliberately not scanned: braces make it Compose's own
/// interpolation spelling, which the generated YAML is read for after
/// `hllc` is finished with it, and no parameter reference has ever
/// looked like that. It needs no special case — `{` simply isn't an
/// identifier character, so the name after that `$` comes out empty and
/// no parameter is ever named `""`.
///
/// Written over [`str::split`] rather than as an index scan on purpose.
/// The obvious hand-rolled version — find a `$`, walk the identifier,
/// resume past it — terminates only because of how its two cursors
/// advance, which makes an off-by-one in either one an infinite loop
/// rather than a wrong answer: `cargo mutants` turns each of those
/// arithmetic operators over in turn and hangs, and the resulting
/// TIMEOUTs would have to be excluded by name in `.cargo/mutants.toml`
/// alongside the lexer's. An iterator over the pieces can't fail to
/// terminate no matter what a mutant does to the body, so there is
/// nothing to exclude.
fn inert_param_refs<'a>(text: &'a str, args: &HashMap<&str, &RawValue>) -> Vec<&'a str> {
    text.split('$')
        // Everything before the first `$` is not after any `$`.
        .skip(1)
        .map(|after_sigil| {
            let end = after_sigil
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(after_sigil.len());
            &after_sigil[..end]
        })
        .filter(|name| args.contains_key(*name))
        .collect()
}

/// Records `warning` unless an identical one is already there.
///
/// Composition substitutes into a *clone* of a template's resolved
/// fields at every call site (see [`resolve_template`]'s cache), so a
/// template invoked by three services walks the same body spans three
/// times and would otherwise report the same problem three times. The
/// list is short enough that a linear scan is cheaper than the set this
/// would otherwise need, and it preserves first-raised order.
fn warn(warnings: &mut Vec<ComposeWarning>, warning: ComposeWarning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

/// [`substitute_literal`], plus a check that the substituted argument is
/// actually reference-shaped — the substitution-time replacement #201
/// gave `substitute_params`' reference-list rows (`networks`,
/// `networks`, `dns`, `env_file`, a `depends_on` entry's own reference)
/// once
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
/// Substitutes every element of a reference-shaped list field,
/// expanding an element that is a `$param` bound to a list into that
/// list's own items (#283).
///
/// `networks $nets` and `networks [$nets]` parse to the same
/// one-element vector, so there is no distinction between them to
/// honour: both splice, and so does `[a, $nets, b]`, which puts the
/// items where the parameter stood. An empty list contributes no
/// elements, which is what an empty list means — unlike an interpolation
/// (see [`interpolated_text`]), where it contributes empty text.
///
/// Each spliced item faces the same two checks a written element does. A
/// nested list or map can't be an element at all, and a bare number is
/// [`ComposeError::ArgumentNotReferenceShaped`] here exactly as it is
/// for a whole-slot substitution — a reference position's own grammar
/// could never hold one, so arriving through a list doesn't make it
/// legal. Both are reported against the *item*, since that is what the
/// author has to change.
fn substitute_reference_list(
    list: &mut Vec<Literal>,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    let mut out = Vec::with_capacity(list.len());
    for mut lit in std::mem::take(list) {
        let bound = match &lit {
            Literal::Param(name, _) => match args.get(name.as_str()) {
                Some(RawValue::List(items, _)) => Some((name.clone(), items)),
                _ => None,
            },
            _ => None,
        };
        let Some((param, items)) = bound else {
            substitute_reference_literal(&mut lit, args, template_name, warnings)?;
            out.push(lit);
            continue;
        };
        for item in items {
            out.push(reference_list_item(item, &param, template_name)?.clone());
        }
    }
    *list = out;
    Ok(())
}

/// One item of a spliced list as the reference it has to be, or the
/// error saying why it isn't (#283).
///
/// Shared by [`substitute_reference_list`] and `depends_on`'s own
/// entry-shaped splice so the two can't come to disagree about what may
/// be spliced. Both refusals name the item rather than the list: a
/// nested list or map can't be one element at all, and a bare number is
/// refused for the same reason [`substitute_reference_literal`] refuses
/// a substituted one — a reference position's grammar could never hold
/// it, so arriving inside a list doesn't make it legal.
fn reference_list_item<'a>(
    item: &'a RawValue,
    param: &str,
    template_name: &str,
) -> Result<&'a Literal, ComposeError> {
    let RawValue::Literal(lit) = item else {
        return Err(ComposeError::TemplateArgumentNotScalar {
            template: template_name.to_string(),
            param: param.to_string(),
            span: item.span(),
        });
    };
    if let Literal::Number { span, .. } = lit {
        return Err(ComposeError::ArgumentNotReferenceShaped {
            template: template_name.to_string(),
            param: param.to_string(),
            span: *span,
        });
    }
    Ok(lit)
}

fn substitute_reference_literal(
    lit: &mut Literal,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    substitute_literal(lit, args, template_name, warnings)?;
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
        // A field access reads a declaration's Docker name, which is
        // text, so a `number`-typed field can't take one however it
        // resolves — worth saying at the argument that passed it rather
        // than reporting the string it becomes a pass later.
        Literal::Field(_) => Some("a field access"),
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
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    let param_name = match lit {
        Literal::Param(name, _) => Some(name.clone()),
        _ => None,
    };
    substitute_literal(lit, args, template_name, warnings)?;
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
    Ok(())
}

/// Substitutes every `Param` reachable inside a schema-free
/// [`RawValue`] tree (a `raw` entry's value, or a nested
/// `with`-invocation's own argument body) — recurses into lists/maps
/// since, unlike a plain `Literal` slot, a `RawValue` position can accept
/// a whole list/map forwarded through unchanged. Never fails (unlike
/// [`substitute_literal`]): a `RawValue` position can hold any argument
/// shape, so there's no "not scalar" case to reject here.
fn substitute_raw_value(
    value: &mut RawValue,
    args: &HashMap<&str, &RawValue>,
    template_name: &str,
    warnings: &mut Vec<ComposeWarning>,
) -> Result<(), ComposeError> {
    let param_name = match value {
        RawValue::Literal(Literal::Param(name, _)) => Some(name.clone()),
        _ => None,
    };
    if let Some(name) = param_name {
        let replacement = args
            .get(name.as_str())
            .expect("param name was already validated against the template's declared params");
        *value = (*replacement).clone();
        return Ok(());
    }
    match value {
        RawValue::List(items, _) => {
            for item in items {
                substitute_raw_value(item, args, template_name, warnings)?;
            }
        }
        RawValue::Map(entries, _) => {
            for (key, v) in entries {
                // A nested map's *keys* interpolate too: `raw` writes
                // arbitrary Compose structure, and codegen resolves
                // `{{name}}` on both sides of every entry it emits
                // (`raw::to_yaml`), so composition has to reach the same
                // slots or the two stages disagree about which halves of
                // a `raw` block a template can parameterize.
                //
                // Only interpolation is ever found here, never a
                // whole-slot `$param`: a key is parsed as a field name,
                // and the grammar has no `$` in that position (`raw {
                // deploy: { $k: "v" } }` is a parse error), so unlike
                // every other row in this walk there is no
                // `Literal::Param` for the substitution half to replace.
                substitute_literal(key, args, template_name, warnings)?;
                substitute_raw_value(v, args, template_name, warnings)?;
            }
        }
        // A literal that isn't a whole-slot `$param` still routes
        // through `substitute_literal`, which is what interpolates
        // `{{param}}` inside its string content (#266).
        RawValue::Literal(lit) => substitute_literal(lit, args, template_name, warnings)?,
    }
    Ok(())
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
/// `devices` isn't among them — see [`Self::arrow_maps`]'s own doc
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
/// `entrypoint` off `expose` entirely, and #271 removed the routing it
/// moved to, so the three rows left here (`networks`/`dns`/`env_file`)
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
    /// — `networks` alone — where naming
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
    // Keyed like `env` above, but with one rule of its own (#288): a
    // list-valued entry concatenates across tiers instead of colliding.
    // See `merge_labels`. The accumulated order is tier order (each
    // `with` target left to right, then the body's own), which is what
    // makes the emitted label order a stable function of the source.
    merge_labels(&mut acc.labels, incoming.labels.entries, tier)?;
    // Keyed by the referenced service's own name, like `env`'s key
    // side — not concatenated through `LIST_FIELDS` above, even though
    // its surface syntax is still a comma/bracket list. Not plain
    // `merge_map` either, unlike `volumes`/`env`/`publish` just above:
    // see `merge_depends_on`'s own doc for the narrower collision rule
    // this field needs.
    merge_depends_on(&mut acc.depends_on, incoming.depends_on, tier)?;
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
/// naming one thing more than once is not an ambiguity between it and
/// itself, it's one answer given twice.
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

/// Merges `labels` entries across tiers, with the one rule that makes
/// composable templates possible again (#288).
///
/// A **list-valued** entry concatenates: several places contributing to
/// one key is what a list means, so a template supplying a base list and
/// another adding to it compose rather than conflict. That is what
/// `router.middleware` did before routing left the compiler, and losing
/// it was the migration's real cost until this. Items dedupe by text —
/// naming one middleware twice is one answer given twice, not two.
///
/// A **scalar-valued** entry keeps [`merge_map`]'s rules exactly: the
/// service's own body overrides a template, and two explicit templates
/// setting it collide. Two answers to a question that takes one answer
/// is the collision #243 exists to catch, and a list is the only way to
/// say the question takes several.
///
/// A list in one tier and a scalar in another is
/// [`ComposeError::LabelShapeMismatch`]: they disagree about what kind
/// of thing the key holds, and either resolution silently discards what
/// the other said.
fn merge_labels(
    acc: &mut Vec<(LabelEntry, Tier)>,
    incoming: Vec<LabelEntry>,
    tier: &Tier,
) -> Result<(), ComposeError> {
    for entry in incoming {
        let key = entry.key.text().to_string();
        let Some(pos) = acc.iter().position(|(e, _)| e.key.text() == key) else {
            acc.push((entry, tier.clone()));
            continue;
        };
        let held_span = acc[pos].0.span;
        let existing_tier = acc[pos].1.clone();
        let mut overridden = None;
        match (&mut acc[pos].0.value, entry.value) {
            (LabelValue::List(held, _), LabelValue::List(items, _)) => {
                for item in items {
                    if !held.iter().any(|h| h.text() == item.text()) {
                        held.push(item);
                    }
                }
                // The accumulated entry keeps the tier it was first seen
                // at: a concatenating entry has no single tier, and
                // nothing downstream asks which one won, because none
                // did.
            }
            (held @ LabelValue::Scalar(_), incoming_value @ LabelValue::Scalar(_)) => {
                match (&existing_tier, tier) {
                    (_, Tier::Own) => {
                        *held = incoming_value;
                        overridden = Some(entry.span);
                    }
                    (Tier::Explicit(first), Tier::Explicit(second)) => {
                        return Err(ComposeError::MapKeyCollision(Box::new(MapKeyCollision {
                            field: "labels",
                            side: MapSide::Key,
                            key,
                            first_template: first.clone(),
                            second_template: second.clone(),
                            first: held_span,
                            second: entry.span,
                        })));
                    }
                    (Tier::Own, Tier::Explicit(_)) => {}
                }
            }
            (held, incoming_value) => {
                return Err(ComposeError::LabelShapeMismatch {
                    key,
                    first_shape: held.shape(),
                    second_shape: incoming_value.shape(),
                    first: held_span,
                    second: entry.span,
                });
            }
        }
        if let Some(span) = overridden {
            acc[pos].0.span = span;
            acc[pos].1 = Tier::Own;
        }
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
