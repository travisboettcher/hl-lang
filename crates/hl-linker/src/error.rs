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
            LinkError::Compose(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LinkError {}
