use crate::span::Span;

/// The kind of a lexical token.
///
/// Per the language's lexical grammar, `template` is the *only* reserved
/// word — it gets its own variant here because the lexical grammar itself
/// reserves it, not because the lexer is guessing at keyword meaning.
/// Every other keyword-shaped word (`service`, `with`, `as`, `external`,
/// `raw`, `defaults`, ...) is deliberately left as [`TokenKind::Ident`]:
/// their meaning is resolved later by the parser against a schema table,
/// and the lexer must not special-case them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `[A-Za-z_][A-Za-z0-9_-]*`, excluding the reserved word `template`.
    Ident,
    /// `[0-9]+` — integers only, no sign, decimal point, or exponent.
    Number,
    /// `"..."` — no escape sequences; cannot contain `"` or a newline.
    Str,
    /// The reserved word `template`.
    Template,
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
    /// Emitted exactly once, at the end of input.
    Eof,
}

/// A single lexical token: its kind, its exact source text, and its
/// location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'src> {
    pub kind: TokenKind,
    /// The token's text. For [`TokenKind::Str`] this is the *decoded*
    /// content only, with the surrounding quotes stripped — see
    /// [`Span`]'s docs for how that makes `lexeme.len()` differ from the
    /// span's byte width for string tokens specifically.
    pub lexeme: &'src str,
    pub span: Span,
}
