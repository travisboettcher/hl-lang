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
                // Every character in the punctuation set the grammar
                // actually uses (`{ } [ ] ( ) : = -> , . $`) already has
                // its own token and never reaches here, so whatever
                // triggers this is essentially always the same real
                // mistake: an unquoted value (a path, a domain, a version
                // string, ...) that needed `"..."` around it (#87).
                write!(
                    f,
                    "{}:{}: unexpected character {ch:?} — string values must be quoted (\"...\")",
                    span.line, span.col
                )
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

#[cfg(test)]
mod display_tests {
    use super::*;

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 2,
            col: 4,
            file: crate::FileId::ANONYMOUS,
        }
    }

    #[test]
    fn unterminated_string_display() {
        let err = LexError::UnterminatedString { span: span() };
        assert_eq!(err.to_string(), "2:4: unterminated string literal");
    }

    #[test]
    fn unexpected_char_display() {
        let err = LexError::UnexpectedChar {
            ch: '/',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "2:4: unexpected character '/' — string values must be quoted (\"...\")"
        );
    }

    #[test]
    fn dangling_dash_display() {
        let err = LexError::DanglingDash { span: span() };
        assert_eq!(
            err.to_string(),
            "2:4: unexpected '-' (expected '->' or an identifier)"
        );
    }
}
