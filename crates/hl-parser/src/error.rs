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
            // `TokenKind`'s own `Display` renders surface syntax (`` `:` ``,
            // "an identifier", ...), not the Rust variant name (#87).
            Expected::Token(kind) => write!(f, "{kind}"),
            Expected::OneOf(kinds) => {
                let parts: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
                write!(f, "one of {}", parts.join(", "))
            }
            Expected::Description(desc) => write!(f, "{desc}"),
        }
    }
}

/// A parse error, structured with position information (mirroring
/// [`LexError`]'s design) so a future machine-readable/JSON diagnostic
/// mode doesn't need to re-derive it from source. Tokenizing collects
/// every [`LexError`] found in one pass (see `Lex`, below), but once
/// parsing itself starts, it still stops at the first error — full
/// parser error recovery is much larger in scope and deliberately out
/// of scope here (#87).
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// One or more lexical errors surfaced while tokenizing (see
    /// [`LexError`]) — every one found in the source, not just the
    /// first (#87), since tokenizing the whole file happens in one pass
    /// before parsing can even start. Always non-empty.
    Lex(Vec<LexError>),
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
    ///
    /// The message names the `raw { <field>: ... }` escape hatch, for
    /// every unrecognized field rather than any particular one (#84):
    /// `raw` passes an arbitrary Compose key through verbatim, so
    /// whenever the unknown name is a real Compose key `hll` has no
    /// dedicated field for yet, that one line is the fix — and a file
    /// written that way keeps compiling unchanged if the field is later
    /// added, since a `raw` key overrides the built-in of the same name.
    /// Without the hint this error was a dead end even when a one-line
    /// workaround existed.
    UnknownField {
        type_name: &'static str,
        field: String,
        /// Whether the body this field was written in is one that
        /// actually accepts a `raw` block, so the hint is only offered
        /// where it would compile. Filled in from the schema
        /// ([`crate::schema::supports_raw`]) rather than by naming
        /// `service`/`template` here, so a future type that gains (or
        /// loses) a schema-free passthrough field stays consistent with
        /// no change to this variant.
        raw_escape_hatch: bool,
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
    /// A `raw` value nested lists/maps deeper than
    /// [`crate::parser::MAX_RAW_VALUE_DEPTH`].
    ///
    /// `raw` is schema-free, so its value grammar is the one genuinely
    /// self-recursive production in the language, and it used to recurse
    /// with no limit at all: a few kilobytes of `[[[[ ... ]]]]` overflowed
    /// the stack and *aborted the process* rather than returning an error
    /// an embedder could catch (#72). The limit is set far below the
    /// depth that overflows, which also keeps the recursive drop of the
    /// resulting `RawValue` tree safe — dropping is the other recursion
    /// here, and it can't return an error at all.
    RawValueTooDeep { limit: usize, span: Span },
    /// A `bare_keyword_alias` fusion (`as`) was immediately followed by a
    /// comma — `expose port as "host", entrypoint: "..."`. The alias
    /// fuses onto the primary value as a one-shot unit and can't itself
    /// continue a field list (docs/DESIGN.md's desugaring rule 3), so
    /// this is always someone reaching for the explicit comma-separated
    /// field form and spelling it with the alias sugar instead. Left
    /// unnamed, this surfaces from the *enclosing* body as a generic
    /// "expected a newline before the next field" error that never
    /// mentions what to write instead (#87).
    AliasSugarCannotContinue {
        type_name: &'static str,
        keyword: &'static str,
        primary_field: &'static str,
        alias_field: &'static str,
        span: Span,
    },
    /// A `volume`/`env` bare entry's first value had neither `:` nor the
    /// type's own bare-entry separator after it. `span` is the entry's
    /// own first value, not wherever parsing next stumbled (often the
    /// start of an unrelated following field, on a different line) —
    /// the missing separator is always this entry's own mistake (#87).
    MapEntryMissingSeparator {
        type_name: &'static str,
        separator: TokenKind,
        span: Span,
    },
}

impl ParseError {
    /// Where the error occurred. For duplicate-style errors this is the
    /// *second* (offending) occurrence; the first occurrence's location
    /// is still available in the variant's `first` field. For `Lex`,
    /// the first (earliest, in source order) of the batched errors.
    pub fn span(&self) -> Span {
        match self {
            ParseError::Lex(errs) => errs
                .first()
                .expect("Lex variant always carries at least one error")
                .span(),
            ParseError::UnexpectedToken { span, .. }
            | ParseError::UnknownTopLevelType { span, .. }
            | ParseError::UnknownField { span, .. }
            | ParseError::DuplicateField { second: span, .. }
            | ParseError::DuplicateMapKey { second: span, .. }
            | ParseError::NumberOutOfRange { span, .. }
            | ParseError::DuplicateTemplateParam { second: span, .. }
            | ParseError::UnknownParamType { span, .. }
            | ParseError::ParamReferenceOutsideTemplate { span, .. }
            | ParseError::UnknownTemplateParam { span, .. }
            | ParseError::RawValueTooDeep { span, .. }
            | ParseError::AliasSugarCannotContinue { span, .. }
            | ParseError::MapEntryMissingSeparator { span, .. } => *span,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span = self.span();
        match self {
            ParseError::Lex(errs) => {
                for (i, err) in errs.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{err}")?;
                }
                Ok(())
            }
            ParseError::UnexpectedToken {
                expected,
                found_kind,
                found_lexeme,
                ..
            } => {
                // A fixed-punctuation kind's surface text already names its
                // one possible lexeme (`` `:` `` can only ever be `:`), so
                // repeating it as a quoted string is redundant; a
                // variable-lexeme kind (identifier/number/string) needs the
                // actual text to be useful (#87).
                match found_kind {
                    TokenKind::Ident | TokenKind::Number | TokenKind::Str => write!(
                        f,
                        "{}:{}: expected {expected}, found {found_kind} {found_lexeme:?}",
                        span.line, span.col
                    ),
                    _ => write!(
                        f,
                        "{}:{}: expected {expected}, found {found_kind}",
                        span.line, span.col
                    ),
                }
            }
            ParseError::UnknownTopLevelType { name, .. } => {
                write!(
                    f,
                    "{}:{}: unknown type {name:?} (expected `network` or `service`)",
                    span.line, span.col
                )
            }
            ParseError::UnknownField {
                type_name,
                field,
                raw_escape_hatch,
                ..
            } => {
                // Offered for every unrecognized name, not a curated
                // list of known-missing Compose keys (#84): `raw` is
                // schema-free, so it's the answer to *whichever* key
                // turns out to be missing, and a typo'd field name costs
                // nothing to see it beside.
                let hint = if *raw_escape_hatch {
                    format!(
                        " — if `{field}` is a Compose key with no `hll` field yet, pass it \
                         through with `raw {{ {field}: ... }}`"
                    )
                } else {
                    String::new()
                };
                write!(
                    f,
                    "{}:{}: unknown field {field:?} on `{type_name}`{hint}",
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
            ParseError::RawValueTooDeep { limit, .. } => write!(
                f,
                "{}:{}: `raw` value nested more than {limit} levels deep",
                span.line, span.col
            ),
            ParseError::AliasSugarCannotContinue {
                type_name,
                keyword,
                primary_field,
                alias_field,
                ..
            } => write!(
                f,
                "{}:{}: `{keyword}` fuses onto the primary value as a one-shot alias and can't \
                 be followed by more fields — write `{type_name} <{primary_field}>, \
                 {alias_field}: \"...\", ...` instead",
                span.line, span.col
            ),
            ParseError::MapEntryMissingSeparator {
                type_name,
                separator,
                ..
            } => write!(
                f,
                "{}:{}: `{type_name}` entry has no `:` or {separator} after its first value",
                span.line, span.col
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod display_tests {
    use super::*;
    use hl_lexer::FileId;

    fn span(line: u32, col: u32) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
            file: FileId::ANONYMOUS,
        }
    }

    #[test]
    fn expected_token_display() {
        assert_eq!(Expected::Token(TokenKind::Colon).to_string(), "`:`");
    }

    #[test]
    fn expected_one_of_display() {
        assert_eq!(
            Expected::OneOf(&[TokenKind::Colon, TokenKind::Equals]).to_string(),
            "one of `:`, `=`"
        );
    }

    #[test]
    fn expected_description_display() {
        assert_eq!(Expected::Description("a value").to_string(), "a value");
    }

    #[test]
    fn lex_display() {
        let err = ParseError::Lex(vec![LexError::DanglingDash { span: span(1, 1) }]);
        assert_eq!(
            err.to_string(),
            "1:1: unexpected '-' (expected '->' or an identifier)"
        );
    }

    #[test]
    fn unexpected_token_display_omits_the_redundant_lexeme_for_punctuation() {
        let err = ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::Colon),
            found_kind: TokenKind::Equals,
            found_lexeme: "=".to_string(),
            span: span(3, 5),
        };
        assert_eq!(err.to_string(), "3:5: expected `:`, found `=`");
    }

    #[test]
    fn unexpected_token_display_keeps_the_lexeme_for_variable_text_kinds() {
        let err = ParseError::UnexpectedToken {
            expected: Expected::Token(TokenKind::Colon),
            found_kind: TokenKind::Number,
            found_lexeme: "42".to_string(),
            span: span(3, 5),
        };
        assert_eq!(err.to_string(), "3:5: expected `:`, found a number \"42\"");
    }

    #[test]
    fn lex_display_joins_multiple_batched_errors_one_per_line() {
        let err = ParseError::Lex(vec![
            LexError::UnexpectedChar {
                ch: '@',
                span: span(1, 1),
            },
            LexError::DanglingDash { span: span(3, 4) },
        ]);
        assert_eq!(
            err.to_string(),
            "1:1: unexpected character '@' — string values must be quoted (\"...\")\n\
             3:4: unexpected '-' (expected '->' or an identifier)"
        );
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

    /// A body that accepts `raw` gets the escape-hatch hint, spelled
    /// with the offending field's own name so it's copy-pasteable (#84).
    #[test]
    fn unknown_field_display_names_the_raw_escape_hatch() {
        let err = ParseError::UnknownField {
            type_name: "service",
            field: "ports".to_string(),
            raw_escape_hatch: true,
            span: span(4, 2),
        };
        assert_eq!(
            err.to_string(),
            "4:2: unknown field \"ports\" on `service` — if `ports` is a Compose key with no \
             `hll` field yet, pass it through with `raw { ports: ... }`"
        );
    }

    /// A body with no `raw` field of its own (`expose`, `image`,
    /// `network`, ...) doesn't get it — writing `raw { ... }` there
    /// wouldn't compile, so suggesting it would just be a second error.
    #[test]
    fn unknown_field_display_omits_the_hint_where_raw_isnt_accepted() {
        let err = ParseError::UnknownField {
            type_name: "expose",
            field: "bogus".to_string(),
            raw_escape_hatch: false,
            span: span(4, 2),
        };
        assert_eq!(err.to_string(), "4:2: unknown field \"bogus\" on `expose`");
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

    #[test]
    fn raw_value_too_deep_display() {
        let err = ParseError::RawValueTooDeep {
            limit: 128,
            span: span(4, 12),
        };
        assert_eq!(
            err.to_string(),
            "4:12: `raw` value nested more than 128 levels deep"
        );
    }
}
