use std::fmt;

use crate::span::Span;

/// The kind of a lexical token.
///
/// The language has no reserved words at all (#258). Every
/// keyword-shaped word — `service`, `template`, `with`, `as`,
/// `external`, `raw`, `defaults`, ... — is deliberately left as
/// [`TokenKind::Ident`]: their meaning is resolved later by the parser,
/// against a schema table or by grammar position, and the lexer must not
/// special-case them.
///
/// `template` was the one exception through #258. It had its own
/// variant, on the reasoning that the lexical grammar itself reserved
/// it — but that reserved it *everywhere*, including the positions where
/// it is just a name, and the parser was already recognizing `use` and
/// `as` by lexeme one grammar rule away. Now it goes the same way; see
/// `hl_parser::Parser::parse_top_decl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `[A-Za-z_][A-Za-z0-9_-]*`. No word is excluded: the language
    /// reserves none.
    Ident,
    /// `[0-9]+` — integers only, no sign, decimal point, or exponent.
    Number,
    /// `"..."` — cannot contain a literal newline; a `"`, a newline, a
    /// tab, a carriage return, or a `\` itself is written as a
    /// backslash escape (see [`crate::unescape`]).
    Str,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Colon,
    Equals,
    /// `->`, always a single token — never two separate `-`/`>` tokens.
    Arrow,
    Comma,
    /// `.` — separates an import alias from the name it qualifies, e.g.
    /// `traefik.traefik-net`. Never appears anywhere else in the
    /// grammar (`NUMBER` is integer-only, so there's no decimal-point
    /// ambiguity to resolve).
    Dot,
    /// `$` — prefixes a template parameter reference (`$port`) inside a
    /// `template`'s own body. Never appears anywhere else in the
    /// grammar; the parser (not the lexer) enforces that it's only legal
    /// in that one context.
    Dollar,
    /// `&&`, always a single token — never two separate `&` tokens, which
    /// the lexer has no variant for at all. Joins two matchers in a
    /// `router`'s `rule` expression (#228).
    AmpAmp,
    /// `||`, always a single token, on [`Self::AmpAmp`]'s reasoning.
    PipePipe,
    /// `!` — negates a matcher, or a parenthesized group of them, in a
    /// `router`'s `rule` expression (#228). Never appears anywhere else
    /// in the grammar.
    Bang,
    /// Emitted exactly once, at the end of input.
    Eof,
}

/// Surface syntax, not the Rust variant name (#87) — used everywhere a
/// diagnostic names a token kind, so a user sees `` `:` `` or "an
/// identifier" rather than `Colon` or `Ident`.
impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            TokenKind::Ident => "an identifier",
            TokenKind::Number => "a number",
            TokenKind::Str => "a string literal",
            TokenKind::LBrace => "`{`",
            TokenKind::RBrace => "`}`",
            TokenKind::LBracket => "`[`",
            TokenKind::RBracket => "`]`",
            TokenKind::LParen => "`(`",
            TokenKind::RParen => "`)`",
            TokenKind::Colon => "`:`",
            TokenKind::Equals => "`=`",
            TokenKind::Arrow => "`->`",
            TokenKind::Comma => "`,`",
            TokenKind::Dot => "`.`",
            TokenKind::Dollar => "`$`",
            TokenKind::AmpAmp => "`&&`",
            TokenKind::PipePipe => "`||`",
            TokenKind::Bang => "`!`",
            TokenKind::Eof => "end of file",
        };
        write!(f, "{text}")
    }
}

/// A single lexical token: its kind, its exact source text, and its
/// location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'src> {
    pub kind: TokenKind,
    /// The token's exact source text. For [`TokenKind::Str`] this is the
    /// content with the surrounding quotes stripped and any backslash
    /// escapes left as they were written — [`crate::unescape`] is what
    /// turns those into the characters they stand for. See [`Span`]'s
    /// docs for how stripping the quotes makes `lexeme.len()` differ
    /// from the span's byte width for string tokens specifically.
    pub lexeme: &'src str,
    pub span: Span,
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn punctuation_renders_as_backtick_quoted_surface_text() {
        assert_eq!(TokenKind::Colon.to_string(), "`:`");
        assert_eq!(TokenKind::Arrow.to_string(), "`->`");
        assert_eq!(TokenKind::LBrace.to_string(), "`{`");
        assert_eq!(TokenKind::AmpAmp.to_string(), "`&&`");
        assert_eq!(TokenKind::PipePipe.to_string(), "`||`");
        assert_eq!(TokenKind::Bang.to_string(), "`!`");
    }

    #[test]
    fn variable_lexeme_kinds_render_as_prose() {
        assert_eq!(TokenKind::Ident.to_string(), "an identifier");
        assert_eq!(TokenKind::Number.to_string(), "a number");
        assert_eq!(TokenKind::Str.to_string(), "a string literal");
    }

    #[test]
    fn eof_renders_as_prose() {
        assert_eq!(TokenKind::Eof.to_string(), "end of file");
    }
}
