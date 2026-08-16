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
    /// A parameter's `: TYPE` annotation named something other than
    /// `Number`/`String` — the only two types this milestone supports.
    UnknownParamType { name: String, span: Span },
    /// A `$name` parameter reference appeared outside a `template`'s own
    /// body (e.g. in a plain `service`), where there's no declared
    /// parameter list to resolve it against.
    ParamReferenceOutsideTemplate { name: String, span: Span },
    /// A `$name` parameter reference appeared inside a template body, but
    /// `name` doesn't match any of that template's own declared
    /// parameters.
    UnknownTemplateParam { name: String, span: Span },
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
            | ParseError::DuplicateTemplateParam { second: span, .. }
            | ParseError::UnknownParamType { span, .. }
            | ParseError::ParamReferenceOutsideTemplate { span, .. }
            | ParseError::UnknownTemplateParam { span, .. } => *span,
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
            ParseError::UnknownParamType { name, .. } => write!(
                f,
                "{}:{}: unknown parameter type `{name}` (expected `Number` or `String`)",
                span.line, span.col
            ),
            ParseError::ParamReferenceOutsideTemplate { name, .. } => write!(
                f,
                "{}:{}: `${name}` is only valid inside a template body",
                span.line, span.col
            ),
            ParseError::UnknownTemplateParam { name, .. } => write!(
                f,
                "{}:{}: `${name}` does not name a declared parameter of this template",
                span.line, span.col
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod display_tests {
    use super::*;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
        }
    }

    #[test]
    fn expected_token_display() {
        assert_eq!(Expected::Token(TokenKind::Colon).to_string(), "Colon");
    }

    #[test]
    fn expected_one_of_display() {
        assert_eq!(
            Expected::OneOf(&[TokenKind::Colon, TokenKind::Equals]).to_string(),
            "one of Colon, Equals"
        );
    }

    #[test]
    fn expected_description_display() {
        assert_eq!(Expected::Description("a value").to_string(), "a value");
    }

    #[test]
    fn lex_display() {
        let err = ParseError::Lex(LexError::DanglingDash { span: span(1, 1) });
        assert_eq!(
            err.to_string(),
            "1:1: unexpected '-' (expected '->' or an identifier)"
        );
    }

    #[test]
    fn unexpected_token_display() {
        let err = ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::Colon),
            found_kind: TokenKind::Equals,
            found_lexeme: "=".to_string(),
            span: span(3, 5),
        };
        assert_eq!(err.to_string(), "3:5: expected Colon, found Equals \"=\"");
    }

    #[test]
    fn unknown_top_level_type_display() {
        let err = ParseError::UnknownTopLevelType {
            name: "widget".to_string(),
            span: span(2, 1),
        };
        assert_eq!(
            err.to_string(),
            "2:1: unknown type \"widget\" (expected `network` or `service`)"
        );
    }

    #[test]
    fn unknown_field_display() {
        let err = ParseError::UnknownField {
            type_name: "service",
            field: "bogus".to_string(),
            span: span(4, 2),
        };
        assert_eq!(err.to_string(), "4:2: unknown field \"bogus\" on `service`");
    }

    #[test]
    fn duplicate_field_display() {
        let err = ParseError::DuplicateField {
            type_name: "service",
            field: "image",
            first: span(1, 3),
            second: span(4, 2),
        };
        assert_eq!(
            err.to_string(),
            "4:2: duplicate field `image` on `service` (first set at 1:3)"
        );
    }

    #[test]
    fn duplicate_map_key_display_key_side() {
        let err = ParseError::DuplicateMapKey {
            type_name: "env",
            side: MapSide::Key,
            value: "FOO".to_string(),
            first: span(1, 1),
            second: span(2, 1),
        };
        assert_eq!(
            err.to_string(),
            "2:1: duplicate `env` entry: key \"FOO\" already set at 1:1"
        );
    }

    #[test]
    fn duplicate_map_key_display_value_side() {
        let err = ParseError::DuplicateMapKey {
            type_name: "env",
            side: MapSide::Value,
            value: "FOO".to_string(),
            first: span(1, 1),
            second: span(2, 1),
        };
        assert_eq!(
            err.to_string(),
            "2:1: duplicate `env` entry: value \"FOO\" already set at 1:1"
        );
    }

    #[test]
    fn number_out_of_range_display() {
        let err = ParseError::NumberOutOfRange {
            text: "99999999999999999999".to_string(),
            span: span(5, 5),
        };
        assert_eq!(
            err.to_string(),
            "5:5: number \"99999999999999999999\" is out of range"
        );
    }

    #[test]
    fn duplicate_template_param_display() {
        let err = ParseError::DuplicateTemplateParam {
            param: "name".to_string(),
            first: span(1, 10),
            second: span(1, 15),
        };
        assert_eq!(
            err.to_string(),
            "1:15: duplicate parameter `name` (first declared at 1:10)"
        );
    }

    #[test]
    fn unknown_param_type_display() {
        let err = ParseError::UnknownParamType {
            name: "Boolean".to_string(),
            span: span(1, 20),
        };
        assert_eq!(
            err.to_string(),
            "1:20: unknown parameter type `Boolean` (expected `Number` or `String`)"
        );
    }

    #[test]
    fn param_reference_outside_template_display() {
        let err = ParseError::ParamReferenceOutsideTemplate {
            name: "port".to_string(),
            span: span(2, 3),
        };
        assert_eq!(
            err.to_string(),
            "2:3: `$port` is only valid inside a template body"
        );
    }

    #[test]
    fn unknown_template_param_display() {
        let err = ParseError::UnknownTemplateParam {
            name: "prot".to_string(),
            span: span(3, 9),
        };
        assert_eq!(
            err.to_string(),
            "3:9: `$prot` does not name a declared parameter of this template"
        );
    }
}
