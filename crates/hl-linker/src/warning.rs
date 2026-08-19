use std::fmt;

use hl_parser::{SourceMap, Span};

/// A non-fatal diagnostic raised while loading a `use` graph: something
/// the user wrote that this stage deliberately drops rather than
/// compiles.
///
/// Warnings are *accumulated*, not returned as an error — [`crate::link`]
/// hands the whole list back alongside the [`hl_parser::ComposedProgram`]
/// it produced (see [`crate::Linked`]), and `hllc` prints them to stderr
/// without touching its exit code (#80). That is the whole of the
/// non-fatal channel: there is deliberately no way to promote one to an
/// error, and no `--quiet`/`-W`/`-A` style flag to suppress one yet —
/// suppression is its own decision, tracked separately, and named
/// variants here are what a later flag would filter on.
///
/// Mirrors [`crate::LinkError`]'s rendering: every location goes through
/// [`Span::locate`], so a warning about a file the user never opened
/// still says which file it is.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkWarning {
    /// An imported (non-entry) file declares a `service`. Only the entry
    /// file's services are compiled — a `use`d file is loaded for its
    /// templates, networks, and volumes, and nothing can reference a
    /// service across files — so this declaration is parsed, checked for
    /// duplicates, and then discarded.
    ImportedService { service: String, span: Span },
    /// An imported (non-entry) file declares `template defaults`. The
    /// implicit `defaults` template is looked up only in the entry
    /// module (see `Graph::resolve_defaults`), because it has no
    /// invocation to carry an alias — there is no `with common.defaults`
    /// to write — so an imported one contributes nothing to any service.
    ImportedDefaults { span: Span },
}

impl LinkWarning {
    /// Where this warning is about, for a caller that wants to render
    /// the location itself.
    pub fn span(&self) -> Span {
        match self {
            LinkWarning::ImportedService { span, .. } | LinkWarning::ImportedDefaults { span } => {
                *span
            }
        }
    }

    /// Renders this warning with its location resolved against `files` —
    /// `path:line:col` instead of a bare `line:col`.
    ///
    /// The map to pass is the one [`crate::link`] attaches to the program
    /// it returns ([`hl_parser::ComposedProgram::files`]). A warning
    /// raised here is *always* about an imported file rather than the one
    /// named on the command line, so naming the file is the point rather
    /// than a nicety.
    pub fn display<'a>(&'a self, files: &'a SourceMap) -> impl fmt::Display + 'a {
        DisplayLinkWarning {
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
            LinkWarning::ImportedService { service, .. } => write!(
                f,
                "{at}: warning: service `{service}` is declared in an imported file and is not \
                 compiled — only the entry file's services are built"
            ),
            LinkWarning::ImportedDefaults { .. } => write!(
                f,
                "{at}: warning: template `defaults` is declared in an imported file and is not \
                 applied — `defaults` is only looked up in the entry file; give it an ordinary \
                 name and apply it with `with`"
            ),
        }
    }
}

/// [`LinkWarning::display`]'s return type: the warning plus the map its
/// span resolves against.
struct DisplayLinkWarning<'a> {
    warning: &'a LinkWarning,
    files: Option<&'a SourceMap>,
}

impl fmt::Display for DisplayLinkWarning<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.warning.write(f, self.files)
    }
}

impl fmt::Display for LinkWarning {
    /// Renders the location as a bare `line:col`, with no file — for a
    /// caller with no [`SourceMap`] in hand. The pipeline always has one
    /// and wants [`LinkWarning::display`].
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
    fn imported_service_display_names_the_file() {
        let mut files = SourceMap::default();
        let lib = files.intern("shared/lib.hll");
        let warning = LinkWarning::ImportedService {
            service: "db".to_string(),
            span: span(4, 1, lib),
        };
        assert_eq!(
            warning.display(&files).to_string(),
            "shared/lib.hll:4:1: warning: service `db` is declared in an imported file and is \
             not compiled — only the entry file's services are built"
        );
    }

    #[test]
    fn imported_defaults_display_falls_back_to_line_col() {
        let warning = LinkWarning::ImportedDefaults {
            span: span(2, 3, FileId::ANONYMOUS),
        };
        assert_eq!(
            warning.to_string(),
            "2:3: warning: template `defaults` is declared in an imported file and is not \
             applied — `defaults` is only looked up in the entry file; give it an ordinary name \
             and apply it with `with`"
        );
    }

    #[test]
    fn span_reports_the_location_it_renders() {
        let warning = LinkWarning::ImportedService {
            service: "db".to_string(),
            span: span(9, 2, FileId::ANONYMOUS),
        };
        assert_eq!(warning.span().line, 9);
        assert_eq!(warning.span().col, 2);
    }
}
