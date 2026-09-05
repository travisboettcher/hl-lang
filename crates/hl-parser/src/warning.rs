use std::fmt;

use hl_lexer::{SourceMap, Span};

/// A non-fatal diagnostic raised while resolving `template`/`with`
/// composition: something the user wrote that composition compiles as
/// asked, but almost certainly not as meant.
///
/// Both variants exist because #266 gave `{{binding}}` a second meaning
/// — a template's own parameter, not just the enclosing service's name —
/// and each names a spelling that was silently *not* that. Warned rather
/// than rejected for the same reason in both cases: the construct has a
/// legitimate reading, it's only the collision with a parameter name
/// that makes it suspicious, and a compiler that hard-errors on
/// suspicion takes working programs down with it.
///
/// Accumulated, not returned as an error — [`fn@crate::compose`] hands the
/// list back on [`crate::ComposedProgram`] and `hllc` prints each to
/// stderr without touching its exit code, exactly as `hl_linker`'s own
/// [`LinkWarning`](../../hl_linker/enum.LinkWarning.html) channel does
/// (#80). There is deliberately no way to promote one to an error and no
/// flag to suppress one yet.
#[derive(Debug, Clone, PartialEq)]
pub enum ComposeWarning {
    /// A string inside a template's body holds `$param` naming one of
    /// that template's own declared parameters — which is not a
    /// parameter reference. `$` substitutes a whole `literal` slot; it
    /// has never been live inside string *content*, so the text passes
    /// through into the generated output verbatim.
    ///
    /// That passthrough is the point. ``"Host(`$host`)"`` compiles, and
    /// emits a Traefik rule matching the literal hostname `$host` — a
    /// router that silently never matches, from source that looks
    /// correct (#65's shape, arrived at from the other direction). Now
    /// that `{{param}}` exists there is a spelling that does what this
    /// one looks like it does, so the two are worth telling apart out
    /// loud.
    ///
    /// Scoped narrowly on purpose: only inside a template body, and only
    /// for `$ident` whose name matches a parameter that template
    /// actually declares. A `$` in any other string is ordinary content
    /// — `command`/`env` legitimately carry `$HOME` through to a shell,
    /// and Compose's own `${VAR}` interpolation reads the generated YAML
    /// after `hllc` is done with it. `${ident}` is never flagged for
    /// that reason: it is unambiguously Compose's spelling, and no
    /// parameter reference has ever looked like it.
    InertParameterInString {
        template: String,
        param: String,
        /// The string literal holding the `$param`, not the parameter's
        /// declaration — the use site is what has to change.
        span: Span,
    },
    /// A template declares a parameter named `name` *and* interpolates
    /// `{{name}}` in its body. Those are two different things with one
    /// spelling, so one of them has to win: `{{name}}` keeps its
    /// original meaning, the enclosing service's own name, and the
    /// parameter is reachable only as `$name`.
    ///
    /// Deciding it that way round is what makes #266 additive: every
    /// `{{name}}` written before parameters could be interpolated at all
    /// still resolves to what it resolved to then, whatever a template
    /// happens to call its parameters.
    NameParameterNotInterpolated {
        template: String,
        /// The string literal holding the `{{name}}`, for the same
        /// reason [`Self::InertParameterInString`] points at one.
        span: Span,
    },
}

impl ComposeWarning {
    /// Where this warning is about, for a caller that wants to render
    /// the location itself.
    pub fn span(&self) -> Span {
        match self {
            ComposeWarning::InertParameterInString { span, .. }
            | ComposeWarning::NameParameterNotInterpolated { span, .. } => *span,
        }
    }

    /// Renders this warning with its location resolved against `files` —
    /// `path:line:col` instead of a bare `line:col`.
    ///
    /// The map to pass is the one `hl_linker`'s `link` attaches to the
    /// program it returns ([`crate::ComposedProgram::files`]). A template
    /// body can live in any file in the `use` graph, so naming the file
    /// is the point rather than a nicety.
    pub fn display<'a>(&'a self, files: &'a SourceMap) -> impl fmt::Display + 'a {
        DisplayComposeWarning {
            warning: self,
            files: Some(files),
        }
    }

    /// The one implementation behind both [`Self::display`] and the
    /// [`Display`](fmt::Display) impl, so the two can't drift apart. The
    /// `warning:` marker is part of the rendered text for the reason
    /// `hl_linker`'s own warning renderer gives: a warning is only ever
    /// reported, so nothing else about the output says what it is.
    fn write(&self, f: &mut fmt::Formatter<'_>, files: Option<&SourceMap>) -> fmt::Result {
        let at = self.span().locate(files);
        match self {
            ComposeWarning::InertParameterInString {
                template, param, ..
            } => write!(
                f,
                "{at}: warning: `${param}` inside a string is not a parameter reference — it is \
                 emitted verbatim; write `{{{{{param}}}}}` to interpolate template \
                 `{template}`'s `{param}` here"
            ),
            ComposeWarning::NameParameterNotInterpolated { template, .. } => write!(
                f,
                "{at}: warning: `{{{{name}}}}` is the enclosing service's name, not template \
                 `{template}`'s `name` parameter — write `$name` for the parameter, or rename it \
                 to interpolate it"
            ),
        }
    }
}

/// [`ComposeWarning::display`]'s return type: the warning plus the map
/// its span resolves against.
struct DisplayComposeWarning<'a> {
    warning: &'a ComposeWarning,
    files: Option<&'a SourceMap>,
}

impl fmt::Display for DisplayComposeWarning<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.warning.write(f, self.files)
    }
}

impl fmt::Display for ComposeWarning {
    /// Renders the location as a bare `line:col`, with no file — for a
    /// caller with no [`SourceMap`] in hand. The pipeline always has one
    /// and wants [`ComposeWarning::display`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_lexer::FileId;

    fn span(line: u32, col: u32, file: FileId) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
            file,
        }
    }

    #[test]
    fn inert_parameter_display_names_the_file() {
        let mut files = SourceMap::default();
        let lib = files.intern("shared/lib.hll");
        let warning = ComposeWarning::InertParameterInString {
            template: "traefik-http".to_string(),
            param: "host".to_string(),
            span: span(3, 43, lib),
        };
        assert_eq!(
            warning.display(&files).to_string(),
            "shared/lib.hll:3:43: warning: `$host` inside a string is not a parameter reference \
             — it is emitted verbatim; write `{{host}}` to interpolate template `traefik-http`'s \
             `host` here"
        );
    }

    #[test]
    fn name_parameter_display_falls_back_to_line_col() {
        let warning = ComposeWarning::NameParameterNotInterpolated {
            template: "t".to_string(),
            span: span(2, 3, FileId::ANONYMOUS),
        };
        assert_eq!(
            warning.to_string(),
            "2:3: warning: `{{name}}` is the enclosing service's name, not template `t`'s `name` \
             parameter — write `$name` for the parameter, or rename it to interpolate it"
        );
    }

    #[test]
    fn span_reports_the_location_it_renders() {
        let warning = ComposeWarning::NameParameterNotInterpolated {
            template: "t".to_string(),
            span: span(9, 2, FileId::ANONYMOUS),
        };
        assert_eq!(warning.span().line, 9);
        assert_eq!(warning.span().col, 2);
    }
}
