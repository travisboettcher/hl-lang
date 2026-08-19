use std::fmt;

use hl_parser::{SourceMap, Span};

/// A non-fatal diagnostic raised while generating Compose YAML:
/// something the user declared that codegen deliberately leaves out of
/// the output.
///
/// Warnings are accumulated rather than returned as an error — they ride
/// out on [`crate::GeneratedProgram::warnings`], and `hllc` prints them
/// to stderr without touching its exit code (#80). There is deliberately
/// no way to promote one to an error, and no `--quiet`/`-W`/`-A` style
/// flag to suppress one yet; suppression is its own decision, tracked
/// separately, and the named variants here are what a later flag would
/// filter on.
///
/// Mirrors [`crate::CodegenError`]'s rendering: every location goes
/// through [`Span::locate`], so a warning about a declaration in an
/// imported file still says which file it is.
#[derive(Debug, Clone, PartialEq)]
pub enum CodegenWarning {
    /// A top-level `network` no service references. The `networks:`
    /// section is built up from what services actually name, so a
    /// declaration nothing reaches never appears in the output at all —
    /// which looks exactly like a typo in a service's `networks [...]`
    /// list, since that misspelling is an error only when the *name*
    /// doesn't resolve, not when the wrong network resolves.
    UnusedNetwork { network: String, span: Span },
}

impl CodegenWarning {
    /// Where this warning is about, for a caller that wants to render
    /// the location itself.
    pub fn span(&self) -> Span {
        match self {
            CodegenWarning::UnusedNetwork { span, .. } => *span,
        }
    }

    /// Renders this warning with its location resolved against `files` —
    /// `path:line:col` instead of a bare `line:col`.
    ///
    /// Codegen runs on an already-composed program whose declarations may
    /// have come from any file in the `use` graph, so the map to pass is
    /// [`hl_parser::ComposedProgram::files`], exactly as for
    /// [`crate::CodegenError::display`].
    pub fn display<'a>(&'a self, files: &'a SourceMap) -> impl fmt::Display + 'a {
        DisplayCodegenWarning {
            warning: self,
            files: Some(files),
        }
    }

    /// The one implementation behind both [`Self::display`] and the
    /// [`Display`](fmt::Display) impl, so the two can't drift apart.
    ///
    /// The `warning:` marker is part of the rendered text rather than
    /// something the CLI prepends: a warning is only ever *reported*, so
    /// unlike an error — which reaches the user as the reason a command
    /// failed — nothing else about the output says what it is. Placing it
    /// after the location follows the same `path:line:col: ...` shape
    /// every diagnostic in the workspace already prints.
    fn write(&self, f: &mut fmt::Formatter<'_>, files: Option<&SourceMap>) -> fmt::Result {
        let at = self.span().locate(files);
        match self {
            CodegenWarning::UnusedNetwork { network, .. } => write!(
                f,
                "{at}: warning: network `{network}` is declared but no service references it, \
                 so it is not emitted — add it to a service's `networks [...]` list, or remove \
                 the declaration"
            ),
        }
    }
}

/// [`CodegenWarning::display`]'s return type: the warning plus the map
/// its span resolves against.
struct DisplayCodegenWarning<'a> {
    warning: &'a CodegenWarning,
    files: Option<&'a SourceMap>,
}

impl fmt::Display for DisplayCodegenWarning<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.warning.write(f, self.files)
    }
}

impl fmt::Display for CodegenWarning {
    /// Renders the location as a bare `line:col`, with no file — for a
    /// caller with no [`SourceMap`] in hand. The pipeline always has one
    /// and wants [`CodegenWarning::display`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_parser::FileId;

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
    fn unused_network_display_names_the_file() {
        let mut files = SourceMap::default();
        let lib = files.intern("shared/network.hll");
        let warning = CodegenWarning::UnusedNetwork {
            network: "proxy".to_string(),
            span: span(1, 1, lib),
        };
        assert_eq!(
            warning.display(&files).to_string(),
            "shared/network.hll:1:1: warning: network `proxy` is declared but no service \
             references it, so it is not emitted — add it to a service's `networks [...]` list, \
             or remove the declaration"
        );
    }

    #[test]
    fn unused_network_display_falls_back_to_line_col() {
        let warning = CodegenWarning::UnusedNetwork {
            network: "proxy".to_string(),
            span: span(3, 5, FileId::ANONYMOUS),
        };
        assert_eq!(
            warning.to_string(),
            "3:5: warning: network `proxy` is declared but no service references it, so it is \
             not emitted — add it to a service's `networks [...]` list, or remove the declaration"
        );
        assert_eq!(warning.span().line, 3);
    }
}
