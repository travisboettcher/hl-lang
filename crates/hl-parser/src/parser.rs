use std::collections::HashMap;

use hl_lexer::{Lexer, Span, Token, TokenKind};

use crate::ast::{
    EnvEntry, EnvMap, Expose, Ident, Image, Literal, Network, Program, RawEntry, RawMap, RawValue,
    Reference, Restart, Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, UseDecl,
    VolumeEntry, VolumeMap,
};
use crate::error::{Expected, ParseError};
use crate::schema::{
    self, FieldKind, FieldResolution, FieldSchema, MapSide, SchemaKind, TypeSchema,
};

/// Parses a complete hl-lang source file into a [`Program`].
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let tokens = Lexer::tokenize(source)?;
    Parser::new(tokens).parse_program()
}

/// One resolved field's accumulated value, keyed by field name inside a
/// struct-kind body. This is the "FieldMap" the generic engine builds up
/// before lowering it into a concrete AST struct once the body finishes.
enum FieldValue {
    Scalar(Literal),
    /// Span of the bare flag token that set it.
    Flag(Span),
    /// A single-occurrence nested struct-kind field (image/expose/restart).
    Struct(StructFields, Span),
    /// An accumulating nested map-kind field (volume/env): (key, value, entry span).
    LiteralMap(Vec<(Literal, Literal, Span)>),
    /// An accumulating nested schema-free map-kind field (raw).
    Raw(RawMap),
    /// An accumulating reference-list field (middleware/depends_on/networks).
    RefList(Vec<Reference>),
    /// An accumulating template-invocation-list field (`with`'s `templates`).
    TemplateInvocations(Vec<TemplateInvocation>),
}

impl FieldValue {
    /// The span of a single-occurrence field's value, used to report the
    /// "first set here" location in a [`ParseError::DuplicateField`].
    /// Only called for `Scalar`/`Flag`/`Struct`, the three kinds that are
    /// ever duplicate-checked — map/list kinds accumulate instead.
    fn span(&self) -> Span {
        match self {
            FieldValue::Scalar(lit) => lit.span(),
            FieldValue::Flag(span) => *span,
            FieldValue::Struct(_, span) => *span,
            FieldValue::LiteralMap(_)
            | FieldValue::Raw(_)
            | FieldValue::RefList(_)
            | FieldValue::TemplateInvocations(_) => {
                unreachable!("map/list-kind fields accumulate and are never duplicate-checked")
            }
        }
    }
}

type StructFields = HashMap<&'static str, FieldValue>;

struct Parser<'src> {
    tokens: Vec<Token<'src>>,
    pos: usize,
}

impl<'src> Parser<'src> {
    fn new(tokens: Vec<Token<'src>>) -> Self {
        Parser { tokens, pos: 0 }
    }

    // ---- token primitives ----

    fn peek(&self) -> &Token<'src> {
        // `tokens` always ends with Eof (Lexer::tokenize guarantees this),
        // and `bump` refuses to advance past it, so this never runs off
        // the end.
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token<'src> {
        let tok = self.tokens[self.pos];
        if self.tokens[self.pos].kind != TokenKind::Eof {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token<'src>, ParseError> {
        let tok = *self.peek();
        if tok.kind == kind {
            self.bump();
            Ok(tok)
        } else {
            Err(self.unexpected(Expected::Token(kind)))
        }
    }

    fn unexpected(&self, expected: Expected) -> ParseError {
        let tok = *self.peek();
        ParseError::UnexpectedToken {
            expected,
            found_kind: tok.kind,
            found_lexeme: tok.lexeme.to_string(),
            span: tok.span,
        }
    }

    fn at_value_start(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::Ident | TokenKind::Str | TokenKind::Number | TokenKind::LBracket
        )
    }

    // ---- literals, keys, references ----

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        let tok = *self.peek();
        match tok.kind {
            TokenKind::Str => {
                self.bump();
                Ok(Literal::Str(tok.lexeme.to_string(), tok.span))
            }
            TokenKind::Ident => {
                self.bump();
                Ok(Literal::Ident(tok.lexeme.to_string(), tok.span))
            }
            TokenKind::Number => {
                self.bump();
                match tok.lexeme.parse::<u64>() {
                    Ok(value) => Ok(Literal::Number {
                        text: tok.lexeme.to_string(),
                        value,
                        span: tok.span,
                    }),
                    Err(_) => Err(ParseError::NumberOutOfRange {
                        text: tok.lexeme.to_string(),
                        span: tok.span,
                    }),
                }
            }
            _ => Err(self.unexpected(Expected::Description(
                "a literal (string, number, or identifier)",
            ))),
        }
    }

    /// `key ::= IDENT | STRING` — unlike a general literal, `NUMBER` is
    /// not a legal field name.
    fn parse_key(&mut self) -> Result<Literal, ParseError> {
        let tok = *self.peek();
        match tok.kind {
            TokenKind::Str => {
                self.bump();
                Ok(Literal::Str(tok.lexeme.to_string(), tok.span))
            }
            TokenKind::Ident => {
                self.bump();
                Ok(Literal::Ident(tok.lexeme.to_string(), tok.span))
            }
            _ => Err(self.unexpected(Expected::Description("a field name (identifier or string)"))),
        }
    }

    /// `reference ::= key ( "." IDENT )?` — the trailing `.IDENT` names an
    /// import alias's declaration (`traefik.traefik-net`). Only a plain
    /// `IDENT` key can be qualified this way — a `STRING` key's content
    /// is just string content, never followed by a structural `.`.
    fn parse_reference(&mut self) -> Result<Reference, ParseError> {
        let key = self.parse_key()?;
        if matches!(key, Literal::Ident(_, _)) && self.peek().kind == TokenKind::Dot {
            self.bump();
            let name_tok = self.expect(TokenKind::Ident)?;
            let qualifier = Ident {
                name: key.text().to_string(),
                span: key.span(),
            };
            let span = Span {
                start: qualifier.span.start,
                end: name_tok.span.end,
                line: qualifier.span.line,
                col: qualifier.span.col,
            };
            return Ok(Reference {
                qualifier: Some(qualifier),
                name: name_tok.lexeme.to_string(),
                name_span: name_tok.span,
                span,
            });
        }
        Ok(Reference {
            qualifier: None,
            name: key.text().to_string(),
            name_span: key.span(),
            span: key.span(),
        })
    }

    fn parse_bracket_reference_list(&mut self) -> Result<Vec<Reference>, ParseError> {
        self.expect(TokenKind::LBracket)?;
        let mut refs = Vec::new();
        if self.peek().kind != TokenKind::RBracket {
            loop {
                refs.push(self.parse_reference()?);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(refs)
    }

    fn parse_bare_reference_list(&mut self) -> Result<Vec<Reference>, ParseError> {
        let mut refs = vec![self.parse_reference()?];
        while self.peek().kind == TokenKind::Comma {
            self.bump();
            refs.push(self.parse_reference()?);
        }
        Ok(refs)
    }

    fn parse_reference_list_value(&mut self) -> Result<Vec<Reference>, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        if self.peek().kind == TokenKind::LBracket {
            self.parse_bracket_reference_list()
        } else if self.at_value_start() {
            self.parse_bare_reference_list()
        } else {
            Err(self.unexpected(Expected::Description("a reference or a list of references")))
        }
    }

    // ---- template invocations (`with`'s `templates` field) ----

    /// `IDENT ( "." IDENT )? ( "{" raw_entry* "}" )?` — one `with`-list
    /// item. A zero-arg invocation (`authenticated`) needs no `{ }`; its
    /// `args` is an empty [`RawMap`]. The name (and optional qualifier)
    /// must be plain `IDENT`s (not the more general `parse_key`, which
    /// also accepts `STRING`) — a `template_decl` or `use` alias can only
    /// ever be named by an `IDENT`, so a string here could never resolve
    /// to a real template.
    fn parse_template_invocation(&mut self) -> Result<TemplateInvocation, ParseError> {
        let first_tok = self.expect(TokenKind::Ident)?;
        let (qualifier, name) = if self.peek().kind == TokenKind::Dot {
            self.bump();
            let name_tok = self.expect(TokenKind::Ident)?;
            (
                Some(Ident {
                    name: first_tok.lexeme.to_string(),
                    span: first_tok.span,
                }),
                Ident {
                    name: name_tok.lexeme.to_string(),
                    span: name_tok.span,
                },
            )
        } else {
            (
                None,
                Ident {
                    name: first_tok.lexeme.to_string(),
                    span: first_tok.span,
                },
            )
        };
        let start_span = qualifier.as_ref().map_or(name.span, |q| q.span);
        let mut end = name.span.end;
        let args = if self.peek().kind == TokenKind::LBrace {
            let raw = self.parse_raw_body()?;
            end = self.tokens[self.pos.saturating_sub(1)].span.end;
            raw
        } else {
            RawMap::default()
        };
        let span = Span {
            start: start_span.start,
            end,
            line: start_span.line,
            col: start_span.col,
        };
        Ok(TemplateInvocation {
            qualifier,
            name,
            args,
            span,
        })
    }

    fn parse_bare_template_invocation_list(
        &mut self,
    ) -> Result<Vec<TemplateInvocation>, ParseError> {
        let mut invs = vec![self.parse_template_invocation()?];
        while self.peek().kind == TokenKind::Comma {
            self.bump();
            invs.push(self.parse_template_invocation()?);
        }
        Ok(invs)
    }

    fn parse_bracket_template_invocation_list(
        &mut self,
    ) -> Result<Vec<TemplateInvocation>, ParseError> {
        self.expect(TokenKind::LBracket)?;
        let mut invs = Vec::new();
        if self.peek().kind != TokenKind::RBracket {
            loop {
                invs.push(self.parse_template_invocation()?);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(invs)
    }

    /// Mirrors [`Self::parse_reference_list_value`]: an optional leading
    /// `:`, then either a bracketed list or the bare comma-list sugar.
    fn parse_template_invocation_list_value(
        &mut self,
    ) -> Result<Vec<TemplateInvocation>, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        if self.peek().kind == TokenKind::LBracket {
            self.parse_bracket_template_invocation_list()
        } else if self.peek().kind == TokenKind::Ident {
            self.parse_bare_template_invocation_list()
        } else {
            Err(self.unexpected(Expected::Description(
                "a template invocation or a list of template invocations",
            )))
        }
    }

    // ---- struct-kind bodies (network/service/image/expose/restart) ----

    fn parse_struct_body(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<(StructFields, Span), ParseError> {
        let open = self.expect(TokenKind::LBrace)?;
        let mut fields = StructFields::new();
        while self.peek().kind != TokenKind::RBrace {
            self.parse_statement_into(schema, &mut fields)?;
            // Per docs/DESIGN.md: "a trailing comma continues a
            // comma-list, its absence unambiguously ends the current
            // statement" — a body's statements need no separator, but an
            // optional comma between them is tolerated too (e.g. a
            // `with`-invocation's argument body: `{ puid: 1000, pgid:
            // 100 }`).
            if self.peek().kind == TokenKind::Comma {
                self.bump();
            }
        }
        let close = self.expect(TokenKind::RBrace)?;
        let span = Span {
            start: open.span.start,
            end: close.span.end,
            line: open.span.line,
            col: open.span.col,
        };
        Ok((fields, span))
    }

    /// The primary-value-shorthand form of a nested struct-kind field,
    /// e.g. `expose 8096 as "host"` instead of `expose { port: 8096, host:
    /// "host" }`. After the primary value, it continues accumulating
    /// trailing bare statements (docs/DESIGN.md's desugaring rule 3) using
    /// pure two-token lookahead: peek the next key (skipping one optional
    /// comma first), and only consume it — comma included, if present —
    /// as part of this nested value if the nested schema actually
    /// resolves it to a real field/alias — otherwise stop and let the
    /// enclosing body parse it (and, if present, the comma before it) as
    /// its own next statement.
    ///
    /// The optional-comma lookahead matters because a comma there reads
    /// as the natural way to visually separate a sugared primary value
    /// from a trailing secondary field (`expose port as "host", entrypoint:
    /// "web-secure"`), but a comma is *also* how the enclosing body's own
    /// next statement can be separated from this one — nothing in the
    /// grammar marks which statement a trailing comma belongs to. Schema
    /// lookup resolves the ambiguity the same way the no-comma case
    /// already does: if what follows genuinely names one of this type's
    /// own fields, the comma belongs here too, rather than silently
    /// detaching the field onto the enclosing block (where it either
    /// misparses as an unrelated statement or fails with a confusing
    /// "unknown field" error that doesn't point at the comma).
    ///
    /// The primary field is usually `Scalar` (one literal), but
    /// docs/DESIGN.md's desugaring rule 1 also anticipates a list-typed
    /// primary field ("a comma-list, if the primary field is
    /// list-typed") — `with`'s `templates` field is the one built-in
    /// case, so this dispatches on the primary field's own [`FieldKind`]
    /// rather than assuming `Scalar`.
    fn parse_struct_primary_shorthand(
        &mut self,
        nested: &'static TypeSchema,
    ) -> Result<(StructFields, Span), ParseError> {
        let primary_name = nested
            .primary_field
            .expect("nested struct types used via bare shorthand must declare a primary field");
        let primary_field = nested
            .fields
            .iter()
            .find(|f| f.name == primary_name)
            .expect("primary_field must name a real field in the type's own field list");
        let mut fields = StructFields::new();

        let start_span = match primary_field.kind {
            FieldKind::TemplateInvocationList => {
                let list_start = self.peek().span;
                let invs = if self.peek().kind == TokenKind::LBracket {
                    self.parse_bracket_template_invocation_list()?
                } else {
                    self.parse_bare_template_invocation_list()?
                };
                fields.insert(primary_name, FieldValue::TemplateInvocations(invs));
                list_start
            }
            _ => {
                let first_value = self.parse_literal()?;
                let span = first_value.span();
                fields.insert(primary_name, FieldValue::Scalar(first_value));
                span
            }
        };

        loop {
            let after_comma = self.peek().kind == TokenKind::Comma;
            let lookahead = if after_comma {
                &self.tokens[self.pos + 1]
            } else {
                self.peek()
            };
            let continues = match lookahead.kind {
                TokenKind::Ident | TokenKind::Str => !matches!(
                    schema::resolve_field(nested, lookahead.lexeme),
                    FieldResolution::Unknown
                ),
                _ => false,
            };
            if !continues {
                break;
            }
            if after_comma {
                self.bump();
            }
            self.parse_statement_into(nested, &mut fields)?;
        }

        let last_end = self.tokens[self.pos.saturating_sub(1)].span.end;
        let span = Span {
            start: start_span.start,
            end: last_end,
            line: start_span.line,
            col: start_span.col,
        };
        Ok((fields, span))
    }

    fn parse_statement_into(
        &mut self,
        schema: &'static TypeSchema,
        fields: &mut StructFields,
    ) -> Result<(), ParseError> {
        let key = self.parse_key()?;
        let key_text = key.text().to_string();
        let key_span = key.span();

        let field = match schema::resolve_field(schema, &key_text) {
            FieldResolution::Unknown => {
                return Err(ParseError::UnknownField {
                    type_name: schema.type_name,
                    field: key_text,
                    span: key_span,
                });
            }
            FieldResolution::RawPassthrough => {
                unreachable!("no struct-kind schema this milestone is schema_free")
            }
            FieldResolution::Field(field) => field,
        };

        match field.kind {
            FieldKind::Scalar => {
                let value = self.parse_field_value_literal()?;
                self.insert_single(schema, fields, field.name, FieldValue::Scalar(value))
            }
            FieldKind::BoolFlag => {
                // Only an explicit `:` is rejected here — a value-start
                // token right after a bare flag isn't an attempted value
                // for *this* flag, it's simply the start of the body's
                // next statement (e.g. `external external` on repeat, or
                // `external image "x"`), which the enclosing loop parses
                // once this call returns.
                if self.peek().kind == TokenKind::Colon {
                    return Err(self.unexpected(Expected::Description(
                        "no value — this flag is set by bare presence only",
                    )));
                }
                self.insert_single(schema, fields, field.name, FieldValue::Flag(key_span))
            }
            FieldKind::Nested(nested) => self.parse_nested_into(schema, field, nested, fields),
            FieldKind::ReferenceList => {
                let refs = self.parse_reference_list_value()?;
                match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::RefList(Vec::new()))
                {
                    FieldValue::RefList(v) => v.extend(refs),
                    _ => unreachable!("field kind is stable for a given field name"),
                }
                Ok(())
            }
            FieldKind::TemplateInvocationList => {
                let invs = self.parse_template_invocation_list_value()?;
                match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::TemplateInvocations(Vec::new()))
                {
                    FieldValue::TemplateInvocations(v) => v.extend(invs),
                    _ => unreachable!("field kind is stable for a given field name"),
                }
                Ok(())
            }
        }
    }

    fn parse_field_value_literal(&mut self) -> Result<Literal, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
            self.parse_literal()
        } else if self.at_value_start() {
            self.parse_literal()
        } else {
            Err(self.unexpected(Expected::Description("a value")))
        }
    }

    fn insert_single(
        &self,
        schema: &'static TypeSchema,
        fields: &mut StructFields,
        name: &'static str,
        value: FieldValue,
    ) -> Result<(), ParseError> {
        if let Some(existing) = fields.get(name) {
            return Err(ParseError::DuplicateField {
                type_name: schema.type_name,
                field: name,
                first: existing.span(),
                second: value.span(),
            });
        }
        fields.insert(name, value);
        Ok(())
    }

    fn parse_nested_into(
        &mut self,
        schema: &'static TypeSchema,
        field: &'static FieldSchema,
        nested: &'static TypeSchema,
        fields: &mut StructFields,
    ) -> Result<(), ParseError> {
        match nested.kind {
            SchemaKind::Struct => {
                let (nested_fields, span) = if self.peek().kind == TokenKind::LBrace {
                    self.parse_struct_body(nested)?
                } else if self.at_value_start() {
                    self.parse_struct_primary_shorthand(nested)?
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                self.insert_single(
                    schema,
                    fields,
                    field.name,
                    FieldValue::Struct(nested_fields, span),
                )
            }
            SchemaKind::Map if nested.schema_free => {
                let raw_map = if self.peek().kind == TokenKind::LBrace {
                    self.parse_raw_body()?
                } else if self.at_value_start() {
                    RawMap {
                        entries: vec![self.parse_raw_entry()?],
                    }
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::Raw(RawMap::default()))
                {
                    FieldValue::Raw(existing) => existing.entries.extend(raw_map.entries),
                    _ => unreachable!("field kind is stable for a given field name"),
                }
                Ok(())
            }
            SchemaKind::Map => {
                let entries = if self.peek().kind == TokenKind::LBrace {
                    self.parse_literal_map_body(nested)?
                } else if self.at_value_start() {
                    vec![self.parse_literal_map_entry(nested)?]
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                merge_literal_map_entries(nested, fields, field.name, entries)
            }
        }
    }

    // ---- literal-valued map-kind bodies (volume/env) ----

    fn parse_literal_map_body(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<Vec<(Literal, Literal, Span)>, ParseError> {
        self.expect(TokenKind::LBrace)?;
        let mut entries = Vec::new();
        while self.peek().kind != TokenKind::RBrace {
            entries.push(self.parse_literal_map_entry(schema)?);
        }
        self.expect(TokenKind::RBrace)?;
        Ok(entries)
    }

    /// One `volume`/`env` entry, in either its canonical form (`key ":"
    /// value`) or its bare-entry sugar form (`value <sep> value`, e.g.
    /// `"host" -> "container"` or `PUID = "1000"`).
    fn parse_literal_map_entry(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<(Literal, Literal, Span), ParseError> {
        let sep = schema
            .map_separator
            .expect("map-kind schema must define a separator");
        let first = self.parse_literal()?;
        if self.peek().kind == TokenKind::Colon || self.peek().kind == sep {
            self.bump();
            let second = self.parse_literal()?;
            let span = Span {
                start: first.span().start,
                end: second.span().end,
                line: first.span().line,
                col: first.span().col,
            };
            Ok((first, second, span))
        } else {
            Err(self.unexpected(Expected::Description(
                "':' or the map's bare-entry separator",
            )))
        }
    }

    // ---- raw (schema-free passthrough) ----

    fn parse_raw_body(&mut self) -> Result<RawMap, ParseError> {
        self.expect(TokenKind::LBrace)?;
        let mut entries = Vec::new();
        while self.peek().kind != TokenKind::RBrace {
            entries.push(self.parse_raw_entry()?);
            // See the matching comment in `parse_struct_body`: an
            // optional trailing comma between entries is tolerated (a
            // `with`-invocation's argument body reuses this parser, and
            // docs/DESIGN.md's worked example writes its multi-argument
            // bodies comma-separated: `{ puid: 1000, pgid: 100 }`).
            if self.peek().kind == TokenKind::Comma {
                self.bump();
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(RawMap { entries })
    }

    /// `raw`'s bare-entry separator is literally `:`, the same token the
    /// canonical `key ":" value` statement form uses — so raw's "sugar"
    /// and "canonical" entry forms are one and the same, with no
    /// distinct code path needed.
    fn parse_raw_entry(&mut self) -> Result<RawEntry, ParseError> {
        let key = self.parse_key()?;
        self.expect(TokenKind::Colon)?;
        let value = self.parse_raw_value()?;
        let span = Span {
            start: key.span().start,
            end: value.span().end,
            line: key.span().line,
            col: key.span().col,
        };
        Ok(RawEntry { key, value, span })
    }

    /// `raw_value ::= literal | list | nested_map` — the one place this
    /// milestone fully implements the grammar's generic `value ::=
    /// literal | list | statement` recursion, since `raw` is schema-free
    /// and has no fixed field list to check values against.
    fn parse_raw_value(&mut self) -> Result<RawValue, ParseError> {
        match self.peek().kind {
            TokenKind::LBracket => {
                let open = self.expect(TokenKind::LBracket)?;
                let mut items = Vec::new();
                if self.peek().kind != TokenKind::RBracket {
                    loop {
                        items.push(self.parse_raw_value()?);
                        if self.peek().kind == TokenKind::Comma {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                }
                let close = self.expect(TokenKind::RBracket)?;
                let span = Span {
                    start: open.span.start,
                    end: close.span.end,
                    line: open.span.line,
                    col: open.span.col,
                };
                Ok(RawValue::List(items, span))
            }
            TokenKind::LBrace => {
                let open = self.expect(TokenKind::LBrace)?;
                let mut entries = Vec::new();
                while self.peek().kind != TokenKind::RBrace {
                    let key = self.parse_key()?;
                    self.expect(TokenKind::Colon)?;
                    let value = self.parse_raw_value()?;
                    entries.push((key, value));
                    if self.peek().kind == TokenKind::Comma {
                        self.bump();
                    }
                }
                let close = self.expect(TokenKind::RBrace)?;
                let span = Span {
                    start: open.span.start,
                    end: close.span.end,
                    line: open.span.line,
                    col: open.span.col,
                };
                Ok(RawValue::Map(entries, span))
            }
            _ => Ok(RawValue::Literal(self.parse_literal()?)),
        }
    }

    // ---- top level ----

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut decls = Vec::new();
        while self.peek().kind != TokenKind::Eof {
            decls.push(self.parse_top_decl()?);
        }
        Ok(Program { decls })
    }

    fn parse_top_decl(&mut self) -> Result<TopDecl, ParseError> {
        if self.peek().kind == TokenKind::Template {
            return Ok(TopDecl::Template(Box::new(self.parse_template_decl()?)));
        }
        if self.peek().kind == TokenKind::Ident && self.peek().lexeme == "use" {
            return Ok(TopDecl::Use(self.parse_use_decl()?));
        }

        let type_tok = self.expect(TokenKind::Ident)?;
        let schema = schema::top_level_type(type_tok.lexeme).ok_or_else(|| {
            ParseError::UnknownTopLevelType {
                name: type_tok.lexeme.to_string(),
                span: type_tok.span,
            }
        })?;

        let name_tok = self.expect(TokenKind::Ident)?;
        let name = Ident {
            name: name_tok.lexeme.to_string(),
            span: name_tok.span,
        };

        let (fields, body_span) = self.parse_struct_body(schema)?;
        let span = Span {
            start: type_tok.span.start,
            end: body_span.end,
            line: type_tok.span.line,
            col: type_tok.span.col,
        };

        match schema.type_name {
            "network" => Ok(TopDecl::Network(lower_network(name, fields, span))),
            "service" => Ok(TopDecl::Service(Box::new(lower_service(
                name, fields, span,
            )))),
            _ => unreachable!("top_level_type only ever returns the network/service schemas"),
        }
    }

    /// `use_decl ::= "use" STRING "as" IDENT`. Neither `use` nor `as` is
    /// lexically reserved — both are ordinary `Ident`s recognized here by
    /// lexeme only, matching `with`/`as`/`external`'s existing precedent
    /// of keeping the reserved-word list as small as possible. `use`'s
    /// path must be a quoted `STRING`: `IDENT`'s grammar can't represent
    /// `.`/`/` at all, so a bare path isn't lexable.
    fn parse_use_decl(&mut self) -> Result<UseDecl, ParseError> {
        let use_tok = self.expect(TokenKind::Ident)?; // lexeme == "use", already peeked
        let path_tok = self.expect(TokenKind::Str)?;
        let path = Literal::Str(path_tok.lexeme.to_string(), path_tok.span);
        if self.peek().kind != TokenKind::Ident || self.peek().lexeme != "as" {
            return Err(self.unexpected(Expected::Description("`as`")));
        }
        self.bump();
        let alias_tok = self.expect(TokenKind::Ident)?;
        let alias = Ident {
            name: alias_tok.lexeme.to_string(),
            span: alias_tok.span,
        };
        let span = Span {
            start: use_tok.span.start,
            end: alias.span.end,
            line: use_tok.span.line,
            col: use_tok.span.col,
        };
        Ok(UseDecl { path, alias, span })
    }

    /// `template_decl ::= "template" IDENT param_list? ( body | "=" statement )`.
    fn parse_template_decl(&mut self) -> Result<TemplateDecl, ParseError> {
        let template_tok = self.expect(TokenKind::Template)?;
        let name_tok = self.expect(TokenKind::Ident)?;
        let name = Ident {
            name: name_tok.lexeme.to_string(),
            span: name_tok.span,
        };

        let params = if self.peek().kind == TokenKind::LParen {
            self.parse_param_list()?
        } else {
            Vec::new()
        };

        let (fields_map, body_span) = if self.peek().kind == TokenKind::Equals {
            self.bump();
            let mut fields = StructFields::new();
            let stmt_start = self.peek().span;
            self.parse_statement_into(&schema::TEMPLATE, &mut fields)?;
            let end = self.tokens[self.pos.saturating_sub(1)].span.end;
            (
                fields,
                Span {
                    start: stmt_start.start,
                    end,
                    line: stmt_start.line,
                    col: stmt_start.col,
                },
            )
        } else if self.peek().kind == TokenKind::LBrace {
            self.parse_struct_body(&schema::TEMPLATE)?
        } else {
            return Err(self.unexpected(Expected::Description("`{` or `=`")));
        };

        let span = Span {
            start: template_tok.span.start,
            end: body_span.end,
            line: template_tok.span.line,
            col: template_tok.span.col,
        };

        let mut decl = TemplateDecl {
            name,
            params,
            fields: lower_service_fields(fields_map),
            span,
        };
        mark_template_params(&mut decl.fields, &decl.params);
        Ok(decl)
    }

    /// `param_list ::= "(" ( IDENT ( "," IDENT )* )? ")"`.
    fn parse_param_list(&mut self) -> Result<Vec<Ident>, ParseError> {
        self.expect(TokenKind::LParen)?;
        let mut params: Vec<Ident> = Vec::new();
        if self.peek().kind != TokenKind::RParen {
            loop {
                let tok = self.expect(TokenKind::Ident)?;
                let param = Ident {
                    name: tok.lexeme.to_string(),
                    span: tok.span,
                };
                if let Some(existing) = params.iter().find(|p| p.name == param.name) {
                    return Err(ParseError::DuplicateTemplateParam {
                        param: param.name,
                        first: existing.span,
                        second: param.span,
                    });
                }
                params.push(param);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok(params)
    }
}

fn merge_literal_map_entries(
    nested: &'static TypeSchema,
    fields: &mut StructFields,
    field_name: &'static str,
    new_entries: Vec<(Literal, Literal, Span)>,
) -> Result<(), ParseError> {
    let side = nested
        .uniqueness
        .expect("volume/env schemas must define a uniqueness side");
    let bucket = match fields
        .entry(field_name)
        .or_insert_with(|| FieldValue::LiteralMap(Vec::new()))
    {
        FieldValue::LiteralMap(v) => v,
        _ => unreachable!("field kind is stable for a given field name"),
    };
    for (key, value, span) in new_entries {
        let check = match side {
            MapSide::Key => key.text(),
            MapSide::Value => value.text(),
        }
        .to_string();
        let dup = bucket.iter().find(|(k, v, _)| {
            let existing = match side {
                MapSide::Key => k.text(),
                MapSide::Value => v.text(),
            };
            existing == check
        });
        if let Some((_, _, first_span)) = dup {
            return Err(ParseError::DuplicateMapKey {
                type_name: nested.type_name,
                side,
                value: check,
                first: *first_span,
                second: span,
            });
        }
        bucket.push((key, value, span));
    }
    Ok(())
}

fn lower_network(name: Ident, mut fields: StructFields, span: Span) -> Network {
    let external = match fields.remove("external") {
        Some(FieldValue::Flag(s)) => Some(s),
        _ => None,
    };
    let real_name = match fields.remove("name") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Network {
        name,
        external,
        real_name,
        span,
    }
}

/// Lowers a raw `StructFields` map into a [`ServiceFields`] — shared by
/// both `lower_service` and `parse_template_decl`, since a `service` body
/// and a `template` body accept exactly the same field set.
fn lower_service_fields(mut fields: StructFields) -> ServiceFields {
    let image = match fields.remove("image") {
        Some(FieldValue::Struct(f, s)) => Some(lower_image(f, s)),
        _ => None,
    };
    let expose = match fields.remove("expose") {
        Some(FieldValue::Struct(f, s)) => Some(lower_expose(f, s)),
        _ => None,
    };
    let restart = match fields.remove("restart") {
        Some(FieldValue::Struct(f, s)) => Some(lower_restart(f, s)),
        _ => None,
    };
    let volumes = match fields.remove("volume") {
        Some(FieldValue::LiteralMap(entries)) => VolumeMap {
            entries: entries
                .into_iter()
                .map(|(host, container, span)| VolumeEntry {
                    host,
                    container,
                    span,
                })
                .collect(),
        },
        _ => VolumeMap::default(),
    };
    let env = match fields.remove("env") {
        Some(FieldValue::LiteralMap(entries)) => EnvMap {
            entries: entries
                .into_iter()
                .map(|(key, value, span)| EnvEntry { key, value, span })
                .collect(),
        },
        _ => EnvMap::default(),
    };
    let raw = match fields.remove("raw") {
        Some(FieldValue::Raw(r)) => r,
        _ => RawMap::default(),
    };
    let middleware = match fields.remove("middleware") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let depends_on = match fields.remove("depends_on") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let networks = match fields.remove("networks") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let with = match fields.remove("with") {
        Some(FieldValue::Struct(mut with_fields, _)) => match with_fields.remove("templates") {
            Some(FieldValue::TemplateInvocations(v)) => v,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    ServiceFields {
        image,
        expose,
        restart,
        volumes,
        env,
        raw,
        middleware,
        depends_on,
        networks,
        with,
    }
}

fn lower_service(name: Ident, fields: StructFields, span: Span) -> Service {
    Service {
        name,
        fields: lower_service_fields(fields),
        span,
    }
}

/// Rewrites every bare-identifier [`Literal`] in `fields` whose text
/// matches one of `params` into [`Literal::Param`] — see that variant's
/// doc for why this is scoped to a single template's own body. Recurses
/// into `fields.with[].args` (nested `with`-invocation argument bodies)
/// so parameter-forwarding through `with X { y: outer_param }` is
/// captured too.
fn mark_template_params(fields: &mut ServiceFields, params: &[Ident]) {
    let is_param = |text: &str| params.iter().any(|p| p.name == text);
    if let Some(img) = &mut fields.image
        && let Some(r) = &mut img.reference
    {
        mark_literal(r, &is_param);
    }
    if let Some(e) = &mut fields.expose {
        if let Some(p) = &mut e.port {
            mark_literal(p, &is_param);
        }
        if let Some(h) = &mut e.host {
            mark_literal(h, &is_param);
        }
    }
    if let Some(r) = &mut fields.restart
        && let Some(p) = &mut r.policy
    {
        mark_literal(p, &is_param);
    }
    for v in &mut fields.volumes.entries {
        mark_literal(&mut v.host, &is_param);
        mark_literal(&mut v.container, &is_param);
    }
    for e in &mut fields.env.entries {
        mark_literal(&mut e.key, &is_param);
        mark_literal(&mut e.value, &is_param);
    }
    for entry in &mut fields.raw.entries {
        mark_literal(&mut entry.key, &is_param);
        mark_raw_value(&mut entry.value, &is_param);
    }
    for inv in &mut fields.with {
        for entry in &mut inv.args.entries {
            mark_literal(&mut entry.key, &is_param);
            mark_raw_value(&mut entry.value, &is_param);
        }
    }
}

fn mark_literal(lit: &mut Literal, is_param: &impl Fn(&str) -> bool) {
    if let Literal::Ident(text, span) = lit
        && is_param(text)
    {
        *lit = Literal::Param(std::mem::take(text), *span);
    }
}

fn mark_raw_value(value: &mut RawValue, is_param: &impl Fn(&str) -> bool) {
    match value {
        RawValue::Literal(lit) => mark_literal(lit, is_param),
        RawValue::List(items, _) => {
            for item in items {
                mark_raw_value(item, is_param);
            }
        }
        RawValue::Map(entries, _) => {
            for (_, v) in entries {
                mark_raw_value(v, is_param);
            }
        }
    }
}

fn lower_image(mut fields: StructFields, span: Span) -> Image {
    let reference = match fields.remove("ref") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Image { reference, span }
}

fn lower_expose(mut fields: StructFields, span: Span) -> Expose {
    let port = match fields.remove("port") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let host = match fields.remove("host") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let entrypoint = match fields.remove("entrypoint") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Expose {
        port,
        host,
        entrypoint,
        span,
    }
}

fn lower_restart(mut fields: StructFields, span: Span) -> Restart {
    let policy = match fields.remove("policy") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Restart { policy, span }
}
