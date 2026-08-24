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
    /// A `\` inside a string literal was followed by a character that
    /// forms no escape sequence (#181).
    ///
    /// A hard error rather than a passthrough of the two characters or a
    /// silent drop of the backslash: either of those would let a literal
    /// mean something other than what it says, which is the failure mode
    /// escapes exist to remove. `span` covers the backslash and the
    /// character after it.
    UnknownEscape { ch: char, span: Span },
    /// A `\` inside a string literal with nothing after it to escape.
    /// Only reachable through [`crate::unescape`] directly — the lexer's
    /// own scan can't produce it, since a `\` before the closing quote
    /// escapes that quote and the literal ends up unterminated instead.
    DanglingEscape { span: Span },
}

impl LexError {
    /// The location the error occurred at.
    pub fn span(&self) -> Span {
        match self {
            LexError::UnterminatedString { span }
            | LexError::UnexpectedChar { span, .. }
            | LexError::DanglingDash { span }
            | LexError::UnknownEscape { span, .. }
            | LexError::DanglingEscape { span } => *span,
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
            // The offending character is rendered through
            // `escape_debug` so a `\` followed by a real tab reads as
            // something a person can see, rather than as a gap in the
            // message.
            LexError::UnknownEscape { ch, .. } => {
                write!(
                    f,
                    "{}:{}: unknown escape sequence `\\{}` — a string literal supports {}",
                    span.line,
                    span.col,
                    ch.escape_debug(),
                    crate::escape::ESCAPE_HINT
                )
            }
            LexError::DanglingEscape { .. } => {
                write!(
                    f,
                    "{}:{}: a string literal can't end with a lone `\\` — write a backslash as `\\\\`",
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
    fn unknown_escape_display_names_the_sequence_and_the_supported_set() {
        let err = LexError::UnknownEscape {
            ch: 'q',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            r#"2:4: unknown escape sequence `\q` — a string literal supports `\"`, `\\`, `\n`, `\t`, and `\r`"#
        );
    }

    /// A `\` followed by a real tab character is still an unknown
    /// escape, and the message has to show it as something visible.
    #[test]
    fn unknown_escape_display_renders_a_control_character_readably() {
        let err = LexError::UnknownEscape {
            ch: '\t',
            span: span(),
        };
        assert!(
            err.to_string()
                .starts_with(r"2:4: unknown escape sequence `\\t`"),
            "{err}"
        );
    }

    #[test]
    fn dangling_escape_display() {
        let err = LexError::DanglingEscape { span: span() };
        assert_eq!(
            err.to_string(),
            r"2:4: a string literal can't end with a lone `\` — write a backslash as `\\`"
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
