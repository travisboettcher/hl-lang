use std::fmt;
use std::path::PathBuf;

use hl_parser::{ComposeError, ParseError, Span};

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
    /// An error from the final [`hl_parser::compose_with_resolver`] pass.
    /// **Known limitation**: the wrapped error's own span(s) may belong
    /// to a *different* file than whichever one first comes to mind —
    /// `Span` carries no file identity, so a compound error spanning two
    /// imported files (e.g. a field collision between a template in one
    /// file and one in another) can't be safely prefixed with a single
    /// path. Rather than guess, this renders the bare underlying message.
    Compose(ComposeError),
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
            LinkError::Compose(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LinkError {}

#[cfg(test)]
mod display_tests {
    use super::*;
    use hl_parser::ComposeError;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
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
    fn compose_display() {
        let err = LinkError::Compose(ComposeError::UnknownTemplate {
            name: "base".to_string(),
            span: span(7, 2),
        });
        assert_eq!(err.to_string(), "7:2: unknown template `base`");
    }
}
