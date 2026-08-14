/// Location of a token in the source text.
///
/// `start`/`end` are 0-indexed byte offsets (end-exclusive) into the
/// original source string. `line`/`col` are 1-indexed and describe the
/// position of `start`; `col` counts Unicode scalar values (`char`s), not
/// bytes or visual width — tabs are not expanded to a tab stop.
///
/// For a [`crate::TokenKind::Str`] token, `span` covers the entire token
/// including both quote characters, while the token's `lexeme` is the
/// quote-stripped inner content — so for string tokens
/// `span.end - span.start == lexeme.len() + 2`.
///
/// The `Eof` token's span is zero-width (`start == end`) at `source.len()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}
