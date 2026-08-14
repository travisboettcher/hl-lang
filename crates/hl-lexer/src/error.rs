use std::fmt;

use crate::span::Span;

/// A lexical error, structured with enough position information to build
/// a machine-readable diagnostic later without re-deriving it from source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexError {
    /// A `"` was opened but never closed before a newline or end of input.
    UnterminatedString { span: Span },
    /// A character that starts no valid token, and isn't whitespace or a
    /// `#` comment.
    UnexpectedChar { ch: char, span: Span },
    /// A `-` was seen that is not the start of `->` — a bare `-` is never
    /// a valid token on its own (it only appears inside an `IDENT`'s tail,
    /// or as the lead character of `->`).
    DanglingDash { span: Span },
}

impl LexError {
    /// The location the error occurred at.
    pub fn span(&self) -> Span {
        match self {
            LexError::UnterminatedString { span }
            | LexError::UnexpectedChar { span, .. }
            | LexError::DanglingDash { span } => *span,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span = self.span();
        match self {
            LexError::UnterminatedString { .. } => {
                write!(f, "{}:{}: unterminated string literal", span.line, span.col)
            }
            LexError::UnexpectedChar { ch, .. } => {
                write!(f, "{}:{}: unexpected character {ch:?}", span.line, span.col)
            }
            LexError::DanglingDash { .. } => {
                write!(
                    f,
                    "{}:{}: unexpected '-' (expected '->' or an identifier)",
                    span.line, span.col
                )
            }
        }
    }
}

impl std::error::Error for LexError {}
