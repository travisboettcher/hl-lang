use std::fmt;

use hl_lexer::{LexError, Span, TokenKind};

use crate::schema::MapSide;

/// What the parser was looking for when it hit an unexpected token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// A single specific token kind was expected.
    Token(TokenKind),
    /// Any one of several token kinds would have been acceptable.
    OneOf(&'static [TokenKind]),
    /// A free-form description, for cases with no single token kind to
    /// name (e.g. "a value", "a field name (identifier or string)").
    Description(&'static str),
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expected::Token(kind) => write!(f, "{kind:?}"),
            Expected::OneOf(kinds) => {
                let parts: Vec<String> = kinds.iter().map(|k| format!("{k:?}")).collect();
                write!(f, "one of {}", parts.join(", "))
            }
            Expected::Description(desc) => write!(f, "{desc}"),
        }
    }
}

/// A parse error, structured with position information (mirroring
/// [`LexError`]'s design) so a future machine-readable/JSON diagnostic
/// mode doesn't need to re-derive it from source. The parser stops at the
/// first error — there is no error recovery this milestone, matching the
/// lexer's own simplicity.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// A lexical error surfaced while tokenizing (see [`LexError`]).
    Lex(LexError),
    /// The next token wasn't one the current grammar position allows.
    UnexpectedToken {
        expected: Expected,
        found_kind: TokenKind,
        found_lexeme: String,
        span: Span,
    },
    /// A top-level declaration's type name isn't `network` or `service`.
    UnknownTopLevelType { name: String, span: Span },
    /// A struct-kind type's body used a field name not in its schema.
    UnknownField {
        type_name: &'static str,
        field: String,
        span: Span,
    },
    /// A single-occurrence field (a scalar, a bare flag, or a
    /// struct-kind nested type) was set more than once in one body.
    DuplicateField {
        type_name: &'static str,
        field: &'static str,
        first: Span,
        second: Span,
    },
    /// Two entries in the same map-kind field (`volume`/`env`) collided
    /// on their schema-configured uniqueness side.
    DuplicateMapKey {
        type_name: &'static str,
        side: MapSide,
        value: String,
        first: Span,
        second: Span,
    },
    /// `NUMBER` is unbounded at the lexical level (`[0-9]+`), but the AST
    /// stores parsed values as `u64`.
    NumberOutOfRange { text: String, span: Span },
    /// A `template`'s `param_list` named the same parameter twice, e.g.
    /// `template foo(a, a) { ... }`.
    DuplicateTemplateParam {
        param: String,
        first: Span,
        second: Span,
    },
}

impl From<LexError> for ParseError {
    fn from(err: LexError) -> Self {
        ParseError::Lex(err)
    }
}

impl ParseError {
    /// Where the error occurred. For duplicate-style errors this is the
    /// *second* (offending) occurrence; the first occurrence's location
    /// is still available in the variant's `first` field.
    pub fn span(&self) -> Span {
        match self {
            ParseError::Lex(err) => err.span(),
            ParseError::UnexpectedToken { span, .. }
            | ParseError::UnknownTopLevelType { span, .. }
            | ParseError::UnknownField { span, .. }
            | ParseError::DuplicateField { second: span, .. }
            | ParseError::DuplicateMapKey { second: span, .. }
            | ParseError::NumberOutOfRange { span, .. }
            | ParseError::DuplicateTemplateParam { second: span, .. } => *span,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span = self.span();
        match self {
            ParseError::Lex(err) => write!(f, "{err}"),
            ParseError::UnexpectedToken {
                expected,
                found_kind,
                found_lexeme,
                ..
            } => write!(
                f,
                "{}:{}: expected {expected}, found {found_kind:?} {found_lexeme:?}",
                span.line, span.col
            ),
            ParseError::UnknownTopLevelType { name, .. } => {
                write!(
                    f,
                    "{}:{}: unknown type {name:?} (expected `network` or `service`)",
                    span.line, span.col
                )
            }
            ParseError::UnknownField {
                type_name, field, ..
            } => {
                write!(
                    f,
                    "{}:{}: unknown field {field:?} on `{type_name}`",
                    span.line, span.col
                )
            }
            ParseError::DuplicateField {
                type_name,
                field,
                first,
                ..
            } => write!(
                f,
                "{}:{}: duplicate field `{field}` on `{type_name}` (first set at {}:{})",
                span.line, span.col, first.line, first.col
            ),
            ParseError::DuplicateMapKey {
                type_name,
                side,
                value,
                first,
                ..
            } => {
                let side_desc = match side {
                    MapSide::Key => "key",
                    MapSide::Value => "value",
                };
                write!(
                    f,
                    "{}:{}: duplicate `{type_name}` entry: {side_desc} {value:?} already set at {}:{}",
                    span.line, span.col, first.line, first.col
                )
            }
            ParseError::NumberOutOfRange { text, .. } => {
                write!(
                    f,
                    "{}:{}: number {text:?} is out of range",
                    span.line, span.col
                )
            }
            ParseError::DuplicateTemplateParam { param, first, .. } => write!(
                f,
                "{}:{}: duplicate parameter `{param}` (first declared at {}:{})",
                span.line, span.col, first.line, first.col
            ),
        }
    }
}

impl std::error::Error for ParseError {}
