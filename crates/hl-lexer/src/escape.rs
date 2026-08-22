use std::borrow::Cow;

use crate::error::LexError;
use crate::span::Span;

/// The escape sequences a `STRING` literal recognizes, as
/// `(source character after the backslash, character it stands for)`.
///
/// Deliberately the small conventional set and nothing more (#181): the
/// two that make a character representable at all (`\"`, `\n`), the one
/// that makes the escape character itself representable (`\\`), and the
/// two other whitespace characters a shell or a config blob asks for in
/// practice (`\t`, `\r`). There is no `\u{...}`, hex, or octal form —
/// a `.hll` file is UTF-8, so a character that isn't one of these five
/// is written as itself.
const ESCAPES: &[(char, char)] = &[
    ('"', '"'),
    ('\\', '\\'),
    ('n', '\n'),
    ('t', '\t'),
    ('r', '\r'),
];

/// The escape set, rendered for a diagnostic. Written out rather than
/// derived from [`ESCAPES`] so the message reads as prose, with the two
/// checked against each other by `the_hint_lists_every_supported_escape`
/// below.
pub(crate) const ESCAPE_HINT: &str = r#"`\"`, `\\`, `\n`, `\t`, and `\r`"#;

/// The character `\<c>` stands for, or `None` if `\<c>` is not an escape
/// sequence this language has.
fn decode(after_backslash: char) -> Option<char> {
    ESCAPES
        .iter()
        .find(|(source, _)| *source == after_backslash)
        .map(|(_, decoded)| *decoded)
}

/// Interprets the backslash escapes in `content`, the quote-stripped
/// source text of a `STRING` token (a [`crate::Token`]'s `lexeme`).
///
/// This is the *only* decoder: [`crate::Lexer`] runs it as it scans each
/// string literal, so an invalid escape is a lex error at the earliest
/// possible moment, and `hl_parser` runs it again to build the decoded
/// value it stores in the AST. Neither one carries its own copy of the
/// escape table.
///
/// `literal` is the span of the whole token, quotes included — what
/// [`crate::Token::span`] holds. It's what an error's own span is
/// measured from, so the diagnostic points at the offending backslash
/// rather than at the start of the literal. A `STRING` can't contain a
/// newline, so every position inside one is on the literal's own line.
///
/// Returns [`Cow::Borrowed`] when `content` has no backslash in it at
/// all, which is nearly every string literal ever written — decoding
/// allocates only for the ones that need it.
///
/// # Errors
///
/// - [`LexError::UnknownEscape`] for a backslash followed by anything
///   outside the supported set. Passing it through as two characters, or
///   dropping the backslash, would make the literal quietly mean
///   something other than what it says.
/// - [`LexError::DanglingEscape`] for a backslash with nothing after it.
///   The lexer never produces such a `lexeme` — a `\` before the closing
///   quote escapes that quote, leaving the literal unterminated — so
///   this is here for any other caller.
///
/// # Example
///
/// ```
/// use hl_lexer::{Lexer, TokenKind, unescape};
///
/// let mut lexer = Lexer::new(r#""line1\nline2""#);
/// let tok = lexer.next_token().unwrap();
/// assert_eq!(tok.kind, TokenKind::Str);
/// assert_eq!(tok.lexeme, r"line1\nline2");
/// assert_eq!(unescape(tok.lexeme, tok.span).unwrap(), "line1\nline2");
/// ```
pub fn unescape(content: &str, literal: Span) -> Result<Cow<'_, str>, LexError> {
    if !content.contains('\\') {
        return Ok(Cow::Borrowed(content));
    }
    let mut out = String::with_capacity(content.len());
    let mut chars = content.char_indices().enumerate();
    while let Some((char_index, (byte_index, ch))) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some((_, (_, escaped))) => match decode(escaped) {
                Some(decoded) => out.push(decoded),
                None => {
                    return Err(LexError::UnknownEscape {
                        ch: escaped,
                        span: escape_span(literal, byte_index, char_index, 1 + escaped.len_utf8()),
                    });
                }
            },
            None => {
                return Err(LexError::DanglingEscape {
                    span: escape_span(literal, byte_index, char_index, 1),
                });
            }
        }
    }
    Ok(Cow::Owned(out))
}

/// The span of one escape sequence inside `literal`, given where its
/// backslash sits in the quote-stripped content and how many bytes the
/// whole sequence covers.
///
/// The `+ 1`s are the opening `"`, which the content offsets are
/// measured past but the literal's own `start`/`col` are not. Byte
/// offsets and columns are counted separately because they disagree the
/// moment a literal holds a non-ASCII character — a `Span`'s `col` is a
/// count of `char`s (see [`Span`]).
fn escape_span(literal: Span, byte_index: usize, char_index: usize, width: usize) -> Span {
    let start = literal
        .start
        .saturating_add(u32::try_from(byte_index + 1).unwrap_or(u32::MAX));
    Span {
        start,
        end: start.saturating_add(u32::try_from(width).unwrap_or(u32::MAX)),
        line: literal.line,
        col: literal
            .col
            .saturating_add(u32::try_from(char_index + 1).unwrap_or(u32::MAX)),
        file: literal.file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::FileId;

    /// A stand-in for the token span of `"<content>"` starting at the
    /// very beginning of a file, which is what the offsets in the tests
    /// below are counted from.
    fn literal(content: &str) -> Span {
        Span {
            start: 0,
            end: u32::try_from(content.len() + 2).unwrap(),
            line: 1,
            col: 1,
            file: FileId::ANONYMOUS,
        }
    }

    fn decoded(content: &str) -> String {
        unescape(content, literal(content)).unwrap().into_owned()
    }

    #[test]
    fn every_supported_escape_decodes_to_its_own_character() {
        assert_eq!(decoded(r#"\""#), "\"");
        assert_eq!(decoded(r"\\"), "\\");
        assert_eq!(decoded(r"\n"), "\n");
        assert_eq!(decoded(r"\t"), "\t");
        assert_eq!(decoded(r"\r"), "\r");
    }

    /// Content with no backslash in it is handed back as-is, without
    /// allocating a second copy of it.
    #[test]
    fn escape_free_content_is_borrowed() {
        let content = "nginx:latest";
        assert!(matches!(
            unescape(content, literal(content)).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    /// A backslash with nothing after it. The lexer can't produce this
    /// lexeme (such a `\` escapes the closing quote, leaving the literal
    /// unterminated), so it's checked against `unescape` directly.
    #[test]
    fn dangling_backslash_is_an_error() {
        let content = r"C:\";
        let err = unescape(content, literal(content)).unwrap_err();
        assert!(matches!(err, LexError::DanglingEscape { .. }), "{err:?}");
    }

    /// The diagnostic's list of supported escapes and the table
    /// [`unescape`] actually decodes against are two statements of the
    /// same set, so an escape added to one and not the other is a bug in
    /// whichever is now lying.
    #[test]
    fn the_hint_lists_every_supported_escape() {
        for (source, _) in ESCAPES {
            assert!(
                ESCAPE_HINT.contains(&format!("`\\{source}`")),
                "escape `\\{source}` is missing from {ESCAPE_HINT}"
            );
        }
        assert_eq!(
            ESCAPE_HINT.matches('`').count(),
            ESCAPES.len() * 2,
            "{ESCAPE_HINT} names something that isn't a supported escape"
        );
    }
}
