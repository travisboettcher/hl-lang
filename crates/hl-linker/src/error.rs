use std::fmt;
use std::path::PathBuf;

use hl_parser::{ComposeError, ParseError, SourceMap, Span};

/// An error raised while loading and linking a `use` graph. Mirrors
/// [`hl_parser::ParseError`]/[`ComposeError`]'s design (structured, no
/// error recovery — loading stops at the first error).
#[derive(Debug, Clone, PartialEq)]
pub enum LinkError {
    /// `path` couldn't be read (missing file, permission error, ...).
    /// `message` is the underlying `io::Error`'s own `Display` text.
    Io { path: PathBuf, message: String },
    /// `path` was read but didn't parse.
    Parse { path: PathBuf, source: ParseError },
    /// Two `use` decls in the same file bind the same alias name.
    /// [`hl_parser::parse`] never checks this — a lone [`hl_parser::Program`]
    /// never needed to, since nothing consumed an alias table until now.
    DuplicateAlias {
        path: PathBuf,
        alias: String,
        first: Span,
        second: Span,
    },
    /// A `use` decl's path is absolute, or climbs (via `..`) above the
    /// root the entry file's own path is relative to. `path` is the
    /// importing file; `raw` is the offending path exactly as written.
    PathEscape {
        path: PathBuf,
        raw: String,
        span: Span,
    },
    /// An error from the final [`hl_parser::compose_with_resolver`]
    /// pass, or from a duplicate declaration caught while building the
    /// module graph.
    ///
    /// Carries the graph's own [`SourceMap`] rather than a single path:
    /// the wrapped error's spans may well belong to *different* files (a
    /// field collision between a template in one imported file and one
    /// in another is the canonical case), and each of them knows which,
    /// so `Display` renders every location as its own `path:line:col`
    /// instead of guessing one path for the lot (#75).
    Compose {
        source: ComposeError,
        files: SourceMap,
    },
}

impl LinkError {
    /// Wraps `source` together with the map its spans resolve against.
    pub(crate) fn compose(source: ComposeError, files: &SourceMap) -> LinkError {
        LinkError::Compose {
            source,
            files: files.clone(),
        }
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Io { path, message } => write!(f, "{}: {message}", path.display()),
            LinkError::Parse { path, source } => write!(f, "{}: {source}", path.display()),
            LinkError::DuplicateAlias {
                path,
                alias,
                first,
                second,
            } => write!(
                f,
                "{}:{}:{}: duplicate alias `{alias}` (first declared at {}:{})",
                path.display(),
                second.line,
                second.col,
                first.line,
                first.col
            ),
            LinkError::PathEscape { path, raw, span } => write!(
                f,
                "{}:{}:{}: import path \"{raw}\" escapes the directory tree rooted at the entry \
                 file (absolute paths and `..` above the root are not allowed)",
                path.display(),
                span.line,
                span.col,
            ),
            LinkError::Compose { source, files } => write!(f, "{}", source.display(files)),
        }
    }
}

impl std::error::Error for LinkError {}

#[cfg(test)]
mod display_tests {
    use super::*;
    use hl_parser::ComposeError;
    use hl_parser::FileId;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
            file: FileId::ANONYMOUS,
        }
    }

    #[test]
    fn io_display() {
        let err = LinkError::Io {
            path: PathBuf::from("services/web.hll"),
            message: "No such file or directory (os error 2)".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "services/web.hll: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn parse_display() {
        let err = LinkError::Parse {
            path: PathBuf::from("services/web.hll"),
            source: ParseError::NumberOutOfRange {
                text: "99999999999999999999".to_string(),
                span: span(2, 4),
            },
        };
        assert_eq!(
            err.to_string(),
            "services/web.hll: 2:4: number \"99999999999999999999\" is out of range"
        );
    }

    #[test]
    fn duplicate_alias_display() {
        let err = LinkError::DuplicateAlias {
            path: PathBuf::from("services/web.hll"),
            alias: "db".to_string(),
            first: span(1, 1),
            second: span(5, 3),
        };
        assert_eq!(
            err.to_string(),
            "services/web.hll:5:3: duplicate alias `db` (first declared at 1:1)"
        );
    }

    #[test]
    fn path_escape_display() {
        let err = LinkError::PathEscape {
            path: PathBuf::from("services/web.hll"),
            raw: "../../../../etc/passwd".to_string(),
            span: span(1, 17),
        };
        assert_eq!(
            err.to_string(),
            "services/web.hll:1:17: import path \"../../../../etc/passwd\" escapes the \
             directory tree rooted at the entry file (absolute paths and `..` above the \
             root are not allowed)"
        );
    }

    #[test]
    fn compose_display_without_a_known_file() {
        let err = LinkError::compose(
            ComposeError::UnknownTemplate {
                name: "base".to_string(),
                span: span(7, 2),
            },
            &SourceMap::default(),
        );
        assert_eq!(err.to_string(), "7:2: unknown template `base`");
    }

    #[test]
    fn compose_display_names_the_file_of_every_location() {
        let mut files = SourceMap::default();
        let one = files.intern("t1.hll");
        let two = files.intern("t2.hll");
        let err = LinkError::compose(
            ComposeError::FieldCollision {
                field: "restart.policy",
                first_template: "x".to_string(),
                second_template: "y".to_string(),
                first: Span {
                    start: 0,
                    end: 0,
                    line: 2,
                    col: 11,
                    file: one,
                },
                second: Span {
                    start: 0,
                    end: 0,
                    line: 2,
                    col: 11,
                    file: two,
                },
            },
            &files,
        );
        assert_eq!(
            err.to_string(),
            "t2.hll:2:11: field `restart.policy` set by both template `x` (at t1.hll:2:11) \
             and template `y` — explicit templates must not conflict"
        );
    }
}
