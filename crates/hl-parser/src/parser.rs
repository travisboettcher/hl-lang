use std::collections::HashMap;

use hl_lexer::{FileId, Lexer, Span, Token, TokenKind};

use crate::ast::{
    DependsOnCondition, DependsOnEntry, EnvEntry, EnvMap, Expose, Healthcheck, HealthcheckTest,
    Ident, Image, Literal, Network, Param, ParamType, Program, PublishEntry, PublishMap, RawEntry,
    RawMap, RawValue, Reference, Restart, Service, ServiceFields, TemplateDecl, TemplateInvocation,
    TopDecl, Traefik, UseDecl, Volume, VolumeDriverOpt, VolumeEntry, VolumeHost, VolumeMap,
};
use crate::error::{Expected, ParseError};
use crate::schema::{
    self, FieldKind, FieldResolution, FieldSchema, MapSide, SchemaKind, TypeSchema,
};

/// How deeply a `raw` value may nest `[`/`{` before
/// [`ParseError::RawValueTooDeep`] stops it.
///
/// `raw`'s schema-free value grammar is the language's one genuinely
/// self-recursive production, so it's the one place a caller controls
/// the parser's stack depth. With no limit, a few kilobytes of
/// `[[[[ ... ]]]]` overflowed the stack, and a stack overflow *aborts
/// the process* — it isn't an error a library embedder can catch, which
/// is what made this worth fixing for crates with public `parse()`/
/// `link()` entry points (#72).
///
/// The margin under the real limit is load-bearing, and not only for
/// parsing: dropping a nested [`RawValue`] recurses through drop glue,
/// and `Drop` has no way to return an error, so the ceiling has to be
/// low enough that *dropping* a maximally deep tree is safe too.
///
/// 128 is picked against measurement rather than by feel. The relevant
/// floor isn't the main thread's 8 MiB stack but a spawned thread's
/// default 2 MiB, since an embedder may well call `parse()` off the main
/// thread: on that stack, a debug build parses and drops 256 levels but
/// aborts at 512. 128 leaves roughly 4× headroom — it survives even a
/// 512 KiB stack — while staying far beyond any legitimate use, since
/// real `raw` blocks mirror Compose YAML and nest a handful of levels at
/// most.
pub const MAX_RAW_VALUE_DEPTH: usize = 128;

/// Parses a complete hl-lang source file into a [`Program`].
///
/// Tokenizing collects every lex error found in the file, not just the
/// first (#87) — since lexing is a whole-file pass that has to finish
/// before parsing can even start, stopping at the first one meant a
/// second, later mistake could sit hidden for another run.
pub fn parse(source: &str) -> Result<Program, ParseError> {
    parse_in_file(source, FileId::ANONYMOUS)
}

/// [`parse`], stamping `file` into every [`Span`] the resulting AST
/// carries.
///
/// This is the entry point the linker uses: it interns each module's
/// path into a [`hl_lexer::SourceMap`] as it loads it and parses that
/// module through here, so a span still knows which file it came from
/// after composition has merged declarations from several modules into
/// one service — which is what lets a diagnostic print `path:line:col`
/// for *each* location it mentions, even when they're in different
/// files (#75).
pub fn parse_in_file(source: &str, file: FileId) -> Result<Program, ParseError> {
    let tokens =
        Lexer::tokenize_collecting_errors_in_file(source, file).map_err(ParseError::Lex)?;
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
    /// An accumulating nested map-kind field (env/publish/driver_opts):
    /// (key, value, entry span).
    LiteralMap(Vec<(Literal, Literal, Span)>),
    /// An accumulating `volume` field: same shape as [`Self::LiteralMap`]
    /// except the key side is a [`VolumeHost`], which the parser has
    /// already split into a bind-mount literal or a named-volume
    /// reference (see [`schema::TypeSchema::key_may_be_reference`]).
    MountMap(Vec<(VolumeHost, Literal, Span)>),
    /// An accumulating nested schema-free map-kind field (raw).
    Raw(RawMap),
    /// An accumulating reference-list field
    /// (middleware/networks/dns/env_file).
    RefList(Vec<Reference>),
    /// An accumulating template-invocation-list field (`with`'s `templates`).
    TemplateInvocations(Vec<TemplateInvocation>),
    /// A single-occurrence field whose value is either a bare literal or
    /// a bracketed list of literals (`healthcheck`'s `test`). See
    /// [`schema::FieldKind::ScalarOrList`]'s doc.
    ScalarOrList(ScalarOrList),
    /// An accumulating `depends_on`-list field. See
    /// [`schema::FieldKind::DependsOnList`]'s doc.
    DependsOnEntries(Vec<DependsOnEntry>),
}

/// The parsed value of a [`schema::FieldKind::ScalarOrList`] field —
/// see that variant's doc for the grammar and why there's no bare
/// comma-list sugar here.
enum ScalarOrList {
    Scalar(Literal),
    /// The list's own span, covering the brackets.
    List(Vec<Literal>, Span),
}

impl ScalarOrList {
    fn span(&self) -> Span {
        match self {
            ScalarOrList::Scalar(lit) => lit.span(),
            ScalarOrList::List(_, span) => *span,
        }
    }
}

impl FieldValue {
    /// The span of a single-occurrence field's value, used to report the
    /// "first set here" location in a [`ParseError::DuplicateField`].
    /// Only called for `Scalar`/`Flag`/`Struct`/`ScalarOrList`, the four
    /// kinds that are ever duplicate-checked — map/list kinds accumulate
    /// instead.
    fn span(&self) -> Span {
        match self {
            FieldValue::Scalar(lit) => lit.span(),
            FieldValue::Flag(span) => *span,
            FieldValue::Struct(_, span) => *span,
            FieldValue::ScalarOrList(v) => v.span(),
            FieldValue::LiteralMap(_)
            | FieldValue::MountMap(_)
            | FieldValue::Raw(_)
            | FieldValue::RefList(_)
            | FieldValue::TemplateInvocations(_)
            | FieldValue::DependsOnEntries(_) => {
                unreachable!("map/list-kind fields accumulate and are never duplicate-checked")
            }
        }
    }
}

type StructFields = HashMap<&'static str, FieldValue>;

struct Parser<'src> {
    tokens: Vec<Token<'src>>,
    pos: usize,
    /// `Some(params)` while parsing a `template`'s own body (including
    /// any nested `with`-invocation argument body written inside it) —
    /// the declared parameter list a `$name` reference is resolved
    /// against. `None` everywhere else (a plain `service`/`network`
    /// body, or a `with`-invocation body written inside one of those),
    /// where a `$name` reference has nothing to resolve against and is a
    /// parse error. Never nested: `template_decl` is only ever a
    /// top-level production, so at most one template body is being
    /// parsed at a time.
    template_params: Option<Vec<Param>>,
}

impl<'src> Parser<'src> {
    fn new(tokens: Vec<Token<'src>>) -> Self {
        Parser {
            tokens,
            pos: 0,
            template_params: None,
        }
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
            TokenKind::Ident
                | TokenKind::Str
                | TokenKind::Number
                | TokenKind::LBracket
                | TokenKind::Dollar
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
            TokenKind::Dollar => self.parse_param_reference(tok),
            _ => Err(self.unexpected(Expected::Description(
                "a literal (string, number, or identifier)",
            ))),
        }
    }

    /// `"$" IDENT` — a template parameter reference. `dollar` is the
    /// already-peeked `$` token; only called from [`Self::parse_literal`],
    /// which hasn't consumed it yet. Resolved immediately against
    /// [`Self::template_params`] rather than deferred to a post-parse
    /// pass: `None` (no enclosing template body) and "name not declared"
    /// are both parse errors here, not composition errors, since neither
    /// depends on anything beyond the current template's own signature.
    fn parse_param_reference(&mut self, dollar: Token<'src>) -> Result<Literal, ParseError> {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident)?;
        let span = Span {
            start: dollar.span.start,
            end: name_tok.span.end,
            line: dollar.span.line,
            col: dollar.span.col,
            file: dollar.span.file,
        };
        let name = name_tok.lexeme.to_string();
        match &self.template_params {
            None => Err(ParseError::ParamReferenceOutsideTemplate { name, span }),
            Some(params) => {
                if params.iter().any(|p| p.name.name == name) {
                    Ok(Literal::Param(name, span))
                } else {
                    Err(ParseError::UnknownTemplateParam { name, span })
                }
            }
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
                file: qualifier.span.file,
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

    /// The unbracketed `a, b, c` form. A comma only continues the list
    /// if what follows it is genuinely another list item — the same
    /// "one-token lookahead decides whether this comma belongs to me or
    /// to whoever called me" rule [`Self::parse_struct_primary_shorthand`]
    /// already applies to its own trailing fields, and needed here for
    /// the same reason now that `expose.entrypoint` is a reference list:
    /// in `expose 8096, entrypoint: web, host: "x"`, the second comma
    /// starts a sibling *field* of `expose`, not a second entry point,
    /// and a greedy list would swallow `host` as one and then fail on
    /// its `:` with an error pointing nowhere near the real problem.
    ///
    /// `KEY :` is the whole tell: a list item is a bare reference
    /// (optionally `alias.name`), never a `key: value` pair, so a colon
    /// one token past the comma can only mean a new field has begun.
    fn parse_bare_reference_list(&mut self) -> Result<Vec<Reference>, ParseError> {
        let mut refs = vec![self.parse_reference()?];
        while self.peek().kind == TokenKind::Comma && !self.comma_starts_a_new_field() {
            self.bump();
            refs.push(self.parse_reference()?);
        }
        Ok(refs)
    }

    /// Whether the `Comma` at the cursor is followed by `KEY :` — see
    /// [`Self::parse_bare_reference_list`]. Safe to index `pos + 1`
    /// (the caller has already seen a non-`Eof` token at `pos`), but
    /// `pos + 2` may be past the end, so that one is checked.
    fn comma_starts_a_new_field(&self) -> bool {
        matches!(
            self.tokens[self.pos + 1].kind,
            TokenKind::Ident | TokenKind::Str
        ) && self
            .tokens
            .get(self.pos + 2)
            .is_some_and(|t| t.kind == TokenKind::Colon)
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

    /// `"[" ( literal ( "," literal )* )? "]"` — the bracketed-list half
    /// of a [`schema::FieldKind::ScalarOrList`] value (`healthcheck`'s
    /// `test`'s exec form, `["CMD", "pg_isready", "-U", "miniflux"]`).
    /// Items are ordinary literals, not [`Reference`]s — unlike
    /// [`Self::parse_bracket_reference_list`], an entry here never
    /// resolves against anything an `.hll` file declares, so it never
    /// carries an `alias.` qualifier either.
    fn parse_bracket_literal_list(&mut self) -> Result<(Vec<Literal>, Span), ParseError> {
        let start = self.expect(TokenKind::LBracket)?.span;
        let mut items = Vec::new();
        if self.peek().kind != TokenKind::RBracket {
            loop {
                items.push(self.parse_literal()?);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let end = self.expect(TokenKind::RBracket)?.span;
        let span = Span {
            start: start.start,
            end: end.end,
            line: start.line,
            col: start.col,
            file: start.file,
        };
        Ok((items, span))
    }

    /// A [`schema::FieldKind::ScalarOrList`] value: an optional leading
    /// `:`, then either a bracketed list of literals or one bare
    /// literal. Deliberately no bare comma-list sugar — see that field
    /// kind's own doc for why.
    fn parse_scalar_or_list_value(&mut self) -> Result<ScalarOrList, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        if self.peek().kind == TokenKind::LBracket {
            let (items, span) = self.parse_bracket_literal_list()?;
            Ok(ScalarOrList::List(items, span))
        } else if self.at_value_start() {
            Ok(ScalarOrList::Scalar(self.parse_literal()?))
        } else {
            Err(self.unexpected(Expected::Description("a value or a bracketed list")))
        }
    }

    // ---- depends_on (#155) ----

    /// `IDENT | STRING` naming one of Compose's own three `depends_on`
    /// conditions: `service_started`, `service_healthy`,
    /// `service_completed_successfully`. Deliberately narrower than
    /// [`Self::parse_literal`] — this position never accepts a `NUMBER`
    /// or a `$param`. The three spellings are fixed Compose keywords,
    /// not a value any homelab would template-parameterize (unlike, say,
    /// `restart.policy`, which is carried through unchecked precisely
    /// because Compose's own legal values there aren't a short, closed
    /// set worth hard-coding) — so there is no later "resolve, then
    /// check" stage this needs to defer to, and checking it here, right
    /// where it's written, gives the best possible span.
    fn parse_depends_on_condition(&mut self) -> Result<(DependsOnCondition, Span), ParseError> {
        let tok = *self.peek();
        if !matches!(tok.kind, TokenKind::Ident | TokenKind::Str) {
            return Err(self.unexpected(Expected::Description(
                "one of `service_started`, `service_healthy`, `service_completed_successfully`",
            )));
        }
        self.bump();
        match DependsOnCondition::parse(tok.lexeme) {
            Some(condition) => Ok((condition, tok.span)),
            None => Err(ParseError::InvalidDependsOnCondition {
                found: tok.lexeme.to_string(),
                span: tok.span,
            }),
        }
    }

    /// `reference ( "{" "condition" ":" condition_value "}" )?` — one
    /// `depends_on`-list item: a plain same-file service reference
    /// (`db`), or a reference plus an explicit condition (`db {
    /// condition: service_healthy }`). Shaped like
    /// [`Self::parse_template_invocation`] — `IDENT` optionally followed
    /// by a `{ }` body — but the body is checked against a real
    /// one-field schema (`condition` is the only legal key) rather than
    /// parsed as a schema-free [`RawMap`], since there's no later stage
    /// that resolves an arbitrary key the way template-argument binding
    /// does.
    fn parse_depends_on_entry(&mut self) -> Result<DependsOnEntry, ParseError> {
        let reference = self.parse_reference()?;
        let mut end = reference.span.end;
        let condition = if self.peek().kind == TokenKind::LBrace {
            self.bump();
            let key_tok = self.expect(TokenKind::Ident)?;
            if key_tok.lexeme != "condition" {
                return Err(ParseError::UnknownField {
                    type_name: "depends_on",
                    field: key_tok.lexeme.to_string(),
                    raw_escape_hatch: false,
                    span: key_tok.span,
                });
            }
            self.expect(TokenKind::Colon)?;
            let value = self.parse_depends_on_condition()?;
            let close = self.expect(TokenKind::RBrace)?;
            end = close.span.end;
            Some(value)
        } else {
            None
        };
        let span = Span {
            start: reference.span.start,
            end,
            line: reference.span.line,
            col: reference.span.col,
            file: reference.span.file,
        };
        Ok(DependsOnEntry {
            reference,
            condition,
            span,
        })
    }

    fn parse_bare_depends_on_list(&mut self) -> Result<Vec<DependsOnEntry>, ParseError> {
        let mut entries = vec![self.parse_depends_on_entry()?];
        while self.peek().kind == TokenKind::Comma && !self.comma_starts_a_new_field() {
            self.bump();
            entries.push(self.parse_depends_on_entry()?);
        }
        Ok(entries)
    }

    fn parse_bracket_depends_on_list(&mut self) -> Result<Vec<DependsOnEntry>, ParseError> {
        self.expect(TokenKind::LBracket)?;
        let mut entries = Vec::new();
        if self.peek().kind != TokenKind::RBracket {
            loop {
                entries.push(self.parse_depends_on_entry()?);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(entries)
    }

    /// Mirrors [`Self::parse_reference_list_value`]: an optional leading
    /// `:`, then either a bracketed list or the bare comma-list sugar.
    fn parse_depends_on_list_value(&mut self) -> Result<Vec<DependsOnEntry>, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        if self.peek().kind == TokenKind::LBracket {
            self.parse_bracket_depends_on_list()
        } else if self.at_value_start() {
            self.parse_bare_depends_on_list()
        } else {
            Err(self.unexpected(Expected::Description(
                "a service reference or a list of service references",
            )))
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
            file: start_span.file,
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
        let mut first = true;
        while self.peek().kind != TokenKind::RBrace {
            // Different fields in a struct-kind body are separated by a
            // newline, never a comma — a comma belongs exclusively to a
            // single field's own comma-list (a bracket list, a `with`
            // invocation list, or a primary-shorthand's own secondary
            // fields; see `parse_struct_primary_shorthand`), never to
            // marking the boundary between two unrelated fields. Only
            // checked from the second field on: a single-statement body
            // (`{ image "foo" }`) needs nothing to separate.
            if !first && self.tokens[self.pos.saturating_sub(1)].span.line == self.peek().span.line
            {
                return Err(
                    self.unexpected(Expected::Description("a newline before the next field"))
                );
            }
            first = false;
            self.parse_statement_into(schema, &mut fields)?;
        }
        let close = self.expect(TokenKind::RBrace)?;
        let span = Span {
            start: open.span.start,
            end: close.span.end,
            line: open.span.line,
            col: open.span.col,
            file: open.span.file,
        };
        Ok((fields, span))
    }

    /// The primary-value-shorthand form of a nested struct-kind field,
    /// e.g. `expose 8096, as: "host"` instead of `expose { port: 8096,
    /// host: "host" }`. After the primary value, it continues
    /// accumulating trailing secondary fields (docs/DESIGN.md's
    /// desugaring rule 3) exactly like any other comma-list: a leading
    /// comma is required to continue, and after it, the next key must
    /// resolve to a real field of the nested type via one-token
    /// lookahead — otherwise stop and let the enclosing body parse
    /// whatever follows (comma included) as its own next statement,
    /// where — since a bare comma never starts a valid field name — it
    /// now correctly errors instead of silently reattaching elsewhere.
    ///
    /// A comma is mandatory here, not optional: unlike an ordinary
    /// top-level field (which is separated from its neighbors by a
    /// newline, never a comma — see `parse_struct_body`), a *secondary*
    /// field is continuing the *same* statement's own value, so it
    /// follows the same "trailing comma continues the list" rule as a
    /// bracket list or a `with`-invocation list, not the "different
    /// fields never share a comma" rule those neighbors follow. Schema
    /// lookup (not the comma alone) is still what confirms the comma
    /// belongs to *this* value rather than to whatever the enclosing body
    /// writes next.
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

        // `nested.bare_keyword_alias` (e.g. `as`) may fuse directly onto
        // the primary value with no comma and no further continuation —
        // `expose port as "host"` is one self-contained unit, not the
        // start of a list. It's deliberately a dead end: if a service
        // needs more than the primary value plus this one aliased field,
        // it must say so explicitly (`expose port, host: "...",
        // entrypoint: "..."`, or the canonical `expose { ... }` body)
        // rather than mixing the alias sugar with further comma-continued
        // fields — `expose port as "host", entrypoint: "..."` no longer
        // parses.
        if let Some((keyword, alias_field)) = nested.bare_keyword_alias
            && self.peek().kind == TokenKind::Ident
            && self.peek().lexeme == keyword
        {
            self.parse_statement_into(nested, &mut fields)?;
            // A comma right here is always someone trying to continue
            // with more fields the way the explicit form allows
            // (`expose port, host: "...", entrypoint: "..."`) but spelled
            // with the alias sugar instead — that combination doesn't
            // parse (see this fn's own doc above), and left to the
            // enclosing body's own newline check, it surfaces as
            // "expected a newline before the next field, found Comma"
            // with no mention of what to write instead (#87). Naming the
            // canonical form directly here is cheap and catches it before
            // that generic message ever gets a chance to fire.
            if self.peek().kind == TokenKind::Comma {
                return Err(ParseError::AliasSugarCannotContinue {
                    type_name: nested.type_name,
                    keyword,
                    primary_field: primary_name,
                    alias_field,
                    span: self.peek().span,
                });
            }
            let last_end = self.tokens[self.pos.saturating_sub(1)].span.end;
            let span = Span {
                start: start_span.start,
                end: last_end,
                line: start_span.line,
                col: start_span.col,
                file: start_span.file,
            };
            return Ok((fields, span));
        }

        // Beyond that, zero or more explicit secondary fields — the same
        // "trailing comma continues, its absence ends the statement" rule
        // every other comma-list in the grammar follows: a comma is
        // required before each one, and one-token lookahead past it
        // confirms the next key genuinely names one of the nested type's
        // own fields (excluding the alias keyword, whose only valid
        // position is the immediate, comma-free one above) before
        // consuming it as part of this value — otherwise the comma (and
        // whatever follows it) is left for the enclosing body, where a
        // bare comma is never a valid statement start and now correctly
        // errors instead of silently reattaching elsewhere.
        loop {
            if self.peek().kind != TokenKind::Comma {
                break;
            }
            let lookahead = &self.tokens[self.pos + 1];
            let is_alias_keyword = nested
                .bare_keyword_alias
                .is_some_and(|(keyword, _)| lookahead.lexeme == keyword);
            let continues = !is_alias_keyword
                && match lookahead.kind {
                    TokenKind::Ident | TokenKind::Str => !matches!(
                        schema::resolve_field(nested, lookahead.lexeme),
                        FieldResolution::Unknown
                    ),
                    _ => false,
                };
            if !continues {
                break;
            }
            self.bump();
            self.parse_statement_into(nested, &mut fields)?;
        }

        let last_end = self.tokens[self.pos.saturating_sub(1)].span.end;
        let span = Span {
            start: start_span.start,
            end: last_end,
            line: start_span.line,
            col: start_span.col,
            file: start_span.file,
        };
        Ok((fields, span))
    }

    /// **Termination invariant** (for [`Self::parse_struct_body`]'s `while
    /// self.peek().kind != TokenKind::RBrace` loop, the sole caller):
    /// every path through this function either bumps at least one token
    /// (via the leading [`Self::parse_key`] call below, on success) or
    /// returns `Err` and unwinds out of the caller's loop entirely via
    /// `?` — so each loop iteration is guaranteed to make progress or
    /// stop. A mutation that replaces this whole function with a no-op
    /// `Ok(())` breaks that: the loop's condition never changes and it
    /// spins forever on any non-empty struct body, which `cargo mutants`
    /// reports as a timeout rather than a normal caught/missed mutant.
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
                    raw_escape_hatch: schema::supports_raw(schema),
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
            FieldKind::ScalarOrList => {
                let value = self.parse_scalar_or_list_value()?;
                self.insert_single(schema, fields, field.name, FieldValue::ScalarOrList(value))
            }
            FieldKind::DependsOnList => {
                let entries = self.parse_depends_on_list_value()?;
                match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::DependsOnEntries(Vec::new()))
                {
                    FieldValue::DependsOnEntries(v) => v.extend(entries),
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
        // Mirrors `parse_field_value_literal` and `parse_reference_list_value`:
        // an optional leading colon (`key: value`) is accepted alongside the
        // bare-sugar form (`key value`).
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        match nested.kind {
            SchemaKind::Struct => {
                // The bare-value shorthand only exists for a type that
                // declares a `primary_field` (docs/DESIGN.md's
                // desugaring rule 1) — `healthcheck` deliberately
                // doesn't (see `schema::HEALTHCHECK`'s doc), so a value
                // token here can only mean the braced body was left off
                // by mistake. Checking that before `at_value_start()`
                // matters: `parse_struct_primary_shorthand` panics if
                // called on a type with no primary field, so this is
                // what keeps that call genuinely unreachable rather than
                // merely untested.
                let (nested_fields, span) = if self.peek().kind == TokenKind::LBrace {
                    self.parse_struct_body(nested)?
                } else if nested.primary_field.is_some() && self.at_value_start() {
                    self.parse_struct_primary_shorthand(nested)?
                } else if nested.primary_field.is_none() {
                    return Err(self.unexpected(Expected::Token(TokenKind::LBrace)));
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
            SchemaKind::Map if nested.key_may_be_reference => {
                let entries = if self.peek().kind == TokenKind::LBrace {
                    self.parse_map_body(|p| p.parse_mount_map_entry(nested))?
                } else if self.at_value_start() {
                    vec![self.parse_mount_map_entry(nested)?]
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                let bucket = match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::MountMap(Vec::new()))
                {
                    FieldValue::MountMap(v) => v,
                    _ => unreachable!("field kind is stable for a given field name"),
                };
                merge_map_entries(nested, bucket, entries, VolumeHost::text)
            }
            SchemaKind::Map => {
                let entries = if self.peek().kind == TokenKind::LBrace {
                    self.parse_literal_map_body(nested)?
                } else if self.at_value_start() {
                    vec![self.parse_literal_map_entry(nested)?]
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                let bucket = match fields
                    .entry(field.name)
                    .or_insert_with(|| FieldValue::LiteralMap(Vec::new()))
                {
                    FieldValue::LiteralMap(v) => v,
                    _ => unreachable!("field kind is stable for a given field name"),
                };
                merge_map_entries(nested, bucket, entries, Literal::text)
            }
        }
    }

    // ---- map-kind bodies (raw / volume / env) ----

    /// Parses a `{ entry (sep entry)* }`-shaped body, shared by `raw {}`
    /// (and, since it reuses that entry parser, a `with`-invocation's own
    /// argument body) and `volume`/`env`'s canonical form. Entries must
    /// be separated by a comma or a newline; bare same-line adjacency
    /// between two entries (`{ a: 1 b: 2 }`) is a parse error, mirroring
    /// the comma-list rule the rest of the language already follows —
    /// "trailing comma continues, its absence ends the statement", never
    /// silent adjacency (#81 follow-up).
    fn parse_map_body<T>(
        &mut self,
        mut parse_entry: impl FnMut(&mut Self) -> Result<T, ParseError>,
    ) -> Result<Vec<T>, ParseError> {
        self.expect(TokenKind::LBrace)?;
        let mut entries = Vec::new();
        while self.peek().kind != TokenKind::RBrace {
            entries.push(parse_entry(self)?);
            let prev_line = self.tokens[self.pos.saturating_sub(1)].span.line;
            if self.peek().kind == TokenKind::Comma {
                self.bump();
            } else if self.peek().kind != TokenKind::RBrace && self.peek().span.line == prev_line {
                return Err(self.unexpected(Expected::Description(
                    "a comma or a newline before the next entry",
                )));
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(entries)
    }

    // ---- literal-valued map-kind bodies (volume/env) ----

    fn parse_literal_map_body(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<Vec<(Literal, Literal, Span)>, ParseError> {
        self.parse_map_body(|p| p.parse_literal_map_entry(schema))
    }

    /// One `env`/`publish`/`driver_opts` entry, in either its canonical
    /// form (`key ":" value`) or its bare-entry sugar form (`value <sep>
    /// value`, e.g. `PUID = "1000"` or `8096 -> 8096`).
    fn parse_literal_map_entry(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<(Literal, Literal, Span), ParseError> {
        let first = self.parse_literal()?;
        let span = first.span();
        self.expect_map_separator(schema, span)?;
        let second = self.parse_literal()?;
        let entry_span = join_spans(span, second.span());
        Ok((first, second, entry_span))
    }

    /// One `volume` entry — the same two forms as
    /// [`Self::parse_literal_map_entry`], except the host side is a
    /// [`VolumeHost`]: a quoted string (or other literal) is a
    /// bind-mount path, and a bare `IDENT` is a reference to a top-level
    /// `volume` declaration, optionally `alias.`-qualified. See
    /// [`VolumeHost`]'s own doc for why that distinction is drawn here,
    /// at parse time, rather than from the string's shape later on.
    fn parse_mount_map_entry(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<(VolumeHost, Literal, Span), ParseError> {
        let host = if self.peek().kind == TokenKind::Ident {
            VolumeHost::Named(self.parse_reference()?)
        } else {
            VolumeHost::BindMount(self.parse_literal()?)
        };
        let span = host.span();
        self.expect_map_separator(schema, span)?;
        let container = self.parse_literal()?;
        let entry_span = join_spans(span, container.span());
        Ok((host, container, entry_span))
    }

    /// Consumes the separator between a map entry's two sides — either
    /// the canonical `:` or the type's own bare-entry separator (`->`
    /// for `volume`/`publish`, `=` for `env`).
    ///
    /// `key_span` is the entry's own first value, which is where a
    /// missing separator is reported, rather than wherever the next
    /// (mismatched) token happens to be — a missing separator is only
    /// ever this entry's own fault, but reporting the *next* token's
    /// position (typically the start of the next field, often on a
    /// different line entirely) reads as if that next field were the
    /// mistake instead (#87). The concrete separator token is named
    /// directly, rather than the schema-internal phrase "the map's
    /// bare-entry separator".
    fn expect_map_separator(
        &mut self,
        schema: &'static TypeSchema,
        key_span: Span,
    ) -> Result<(), ParseError> {
        let sep = schema
            .map_separator
            .expect("map-kind schema must define a separator");
        if self.peek().kind == TokenKind::Colon || self.peek().kind == sep {
            self.bump();
            Ok(())
        } else {
            Err(ParseError::MapEntryMissingSeparator {
                type_name: schema.type_name,
                separator: sep,
                span: key_span,
            })
        }
    }

    // ---- raw (schema-free passthrough) ----

    fn parse_raw_body(&mut self) -> Result<RawMap, ParseError> {
        let entries = self.parse_map_body(Self::parse_raw_entry)?;
        Ok(RawMap { entries })
    }

    /// `raw`'s bare-entry separator is literally `:`, the same token the
    /// canonical `key ":" value` statement form uses — so raw's "sugar"
    /// and "canonical" entry forms are one and the same, with no
    /// distinct code path needed.
    fn parse_raw_entry(&mut self) -> Result<RawEntry, ParseError> {
        let key = self.parse_key()?;
        self.expect(TokenKind::Colon)?;
        let value = self.parse_raw_value(0)?;
        let span = Span {
            start: key.span().start,
            end: value.span().end,
            line: key.span().line,
            col: key.span().col,
            file: key.span().file,
        };
        Ok(RawEntry { key, value, span })
    }

    /// `raw_value ::= literal | list | nested_map` — the one place this
    /// milestone fully implements the grammar's generic `value ::=
    /// literal | list | statement` recursion, since `raw` is schema-free
    /// and has no fixed field list to check values against.
    ///
    /// `depth` is how many enclosing `[`/`{` this call sits inside, and
    /// is capped at [`MAX_RAW_VALUE_DEPTH`]: this is real recursion over
    /// attacker-shaped input, and without a limit a few kilobytes of
    /// `[[[[ ... ]]]]` overflowed the stack and aborted the process
    /// (#72). Every entry point passes `0`.
    ///
    /// The check is written against `level` — the 1-based level this
    /// call itself occupies — rather than as `depth >= MAX`, which is
    /// the same test but has an *equivalent mutant*: because `depth`
    /// only ever rises one at a time, `==` and `>=` trigger at exactly
    /// the same call, so no test could tell them apart and `cargo
    /// mutants` reports the swap as missed coverage forever. Comparing
    /// `level > MAX` moves every comparison mutant one level off the
    /// real boundary, where the tests at and past the limit catch it.
    fn parse_raw_value(&mut self, depth: usize) -> Result<RawValue, ParseError> {
        let level = depth + 1;
        if level > MAX_RAW_VALUE_DEPTH {
            return Err(ParseError::RawValueTooDeep {
                limit: MAX_RAW_VALUE_DEPTH,
                span: self.peek().span,
            });
        }
        match self.peek().kind {
            TokenKind::LBracket => {
                let open = self.expect(TokenKind::LBracket)?;
                let mut items = Vec::new();
                if self.peek().kind != TokenKind::RBracket {
                    loop {
                        items.push(self.parse_raw_value(depth + 1)?);
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
                    file: open.span.file,
                };
                Ok(RawValue::List(items, span))
            }
            TokenKind::LBrace => {
                let open = self.expect(TokenKind::LBrace)?;
                let mut entries = Vec::new();
                while self.peek().kind != TokenKind::RBrace {
                    let key = self.parse_key()?;
                    self.expect(TokenKind::Colon)?;
                    let value = self.parse_raw_value(depth + 1)?;
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
                    file: open.span.file,
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
            file: type_tok.span.file,
        };

        match schema.type_name {
            "network" => Ok(TopDecl::Network(lower_network(name, fields, span))),
            "volume" => Ok(TopDecl::Volume(lower_volume(name, fields, span))),
            "service" => Ok(TopDecl::Service(Box::new(lower_service(
                name, fields, span,
            )))),
            _ => {
                unreachable!("top_level_type only ever returns the network/volume/service schemas")
            }
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
            file: use_tok.span.file,
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

        // Every `$name` reference inside the body below (including a
        // nested `with`-invocation's own argument body) resolves against
        // this template's own just-parsed `params` — see
        // `Self::template_params`'s doc. Cleared again once the body is
        // fully parsed; `template_decl` is never nested, so there's no
        // outer value to restore instead of `None`.
        self.template_params = Some(params.clone());
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
                    file: stmt_start.file,
                },
            )
        } else if self.peek().kind == TokenKind::LBrace {
            self.parse_struct_body(&schema::TEMPLATE)?
        } else {
            return Err(self.unexpected(Expected::Description("`{` or `=`")));
        };
        self.template_params = None;

        let span = Span {
            start: template_tok.span.start,
            end: body_span.end,
            line: template_tok.span.line,
            col: template_tok.span.col,
            file: template_tok.span.file,
        };

        Ok(TemplateDecl {
            name,
            params,
            fields: lower_service_fields(fields_map),
            span,
        })
    }

    /// `param_list ::= "(" ( param ( "," param )* )? ")"`,
    /// `param ::= IDENT ( ":" param_type )?`,
    /// `param_type ::= "Number" | "String"`.
    fn parse_param_list(&mut self) -> Result<Vec<Param>, ParseError> {
        self.expect(TokenKind::LParen)?;
        let mut params: Vec<Param> = Vec::new();
        if self.peek().kind != TokenKind::RParen {
            loop {
                let tok = self.expect(TokenKind::Ident)?;
                let name = Ident {
                    name: tok.lexeme.to_string(),
                    span: tok.span,
                };
                if let Some(existing) = params.iter().find(|p| p.name.name == name.name) {
                    return Err(ParseError::DuplicateTemplateParam {
                        param: name.name,
                        first: existing.name.span,
                        second: name.span,
                    });
                }
                let ty = if self.peek().kind == TokenKind::Colon {
                    self.bump();
                    Some(self.parse_param_type()?)
                } else {
                    None
                };
                params.push(Param { name, ty });
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

    /// `param_type ::= "Number" | "String"` — the only two type names a
    /// parameter's `:` annotation may name this milestone.
    fn parse_param_type(&mut self) -> Result<ParamType, ParseError> {
        let tok = self.expect(TokenKind::Ident)?;
        match tok.lexeme {
            "Number" => Ok(ParamType::Number),
            "String" => Ok(ParamType::String),
            _ => Err(ParseError::UnknownParamType {
                name: tok.lexeme.to_string(),
                span: tok.span,
            }),
        }
    }
}

/// Joins two spans into one covering both, taking the line/col/file of
/// the first — the shape every multi-token construct in this parser
/// builds by hand.
fn join_spans(start: Span, end: Span) -> Span {
    Span {
        start: start.start,
        end: end.end,
        line: start.line,
        col: start.col,
        file: start.file,
    }
}

/// Accumulates one map-kind field's newly parsed entries into whatever
/// earlier writes of the same field already contributed, rejecting a
/// duplicate on whichever side [`TypeSchema::uniqueness`] names.
///
/// Generic over the key side's own type so one routine serves every
/// map-kind field: `env`/`publish`/`driver_opts` key on a [`Literal`],
/// `volume` on a [`VolumeHost`]. `key_text` reads that side's text —
/// `Literal::text` or `VolumeHost::text`, passed by the caller, since
/// there is no one trait both already implement.
fn merge_map_entries<K>(
    nested: &'static TypeSchema,
    bucket: &mut Vec<(K, Literal, Span)>,
    new_entries: Vec<(K, Literal, Span)>,
    key_text: fn(&K) -> &str,
) -> Result<(), ParseError> {
    let side = nested
        .uniqueness
        .expect("volume/env schemas must define a uniqueness side");
    for (key, value, span) in new_entries {
        let check = match side {
            MapSide::Key => key_text(&key),
            MapSide::Value => value.text(),
        }
        .to_string();
        let dup = bucket.iter().find(|(k, v, _)| {
            let existing = match side {
                MapSide::Key => key_text(k),
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

/// Lowers a top-level `volume` declaration's body. Deliberately shaped
/// like [`lower_network`] — `external`/`name` are read exactly the same
/// way — plus the two volume-only Compose knobs.
fn lower_volume(name: Ident, mut fields: StructFields, span: Span) -> Volume {
    let external = match fields.remove("external") {
        Some(FieldValue::Flag(s)) => Some(s),
        _ => None,
    };
    let real_name = match fields.remove("name") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let driver = match fields.remove("driver") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let driver_opts = match fields.remove("driver_opts") {
        Some(FieldValue::LiteralMap(entries)) => entries
            .into_iter()
            .map(|(key, value, span)| VolumeDriverOpt { key, value, span })
            .collect(),
        _ => Vec::new(),
    };
    Volume {
        name,
        external,
        real_name,
        driver,
        driver_opts,
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
    let traefik = match fields.remove("traefik") {
        Some(FieldValue::Struct(f, s)) => Some(lower_traefik(f, s)),
        _ => None,
    };
    let restart = match fields.remove("restart") {
        Some(FieldValue::Struct(f, s)) => Some(lower_restart(f, s)),
        _ => None,
    };
    let healthcheck = match fields.remove("healthcheck") {
        Some(FieldValue::Struct(f, s)) => Some(lower_healthcheck(f, s)),
        _ => None,
    };
    let container_name = match fields.remove("container_name") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let publish = match fields.remove("publish") {
        Some(FieldValue::LiteralMap(entries)) => PublishMap {
            entries: entries
                .into_iter()
                .map(|(host, container, span)| PublishEntry {
                    host,
                    container,
                    span,
                })
                .collect(),
        },
        _ => PublishMap::default(),
    };
    let volumes = match fields.remove("volume") {
        Some(FieldValue::MountMap(entries)) => VolumeMap {
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
        Some(FieldValue::DependsOnEntries(v)) => v,
        _ => Vec::new(),
    };
    let networks = match fields.remove("networks") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let dns = match fields.remove("dns") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let env_file = match fields.remove("env_file") {
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
        traefik,
        restart,
        healthcheck,
        publish,
        volumes,
        env,
        raw,
        middleware,
        depends_on,
        networks,
        dns,
        env_file,
        container_name,
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
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
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

/// Lowers a `traefik { ... }` body (#159). Mirrors `healthcheck`'s
/// `disable` extraction exactly — see [`Traefik::disabled`]'s doc.
fn lower_traefik(mut fields: StructFields, span: Span) -> Traefik {
    let disabled = match fields.remove("disabled") {
        Some(FieldValue::Flag(s)) => Some(s),
        _ => None,
    };
    Traefik { disabled, span }
}

fn lower_healthcheck(mut fields: StructFields, span: Span) -> Healthcheck {
    let test = match fields.remove("test") {
        Some(FieldValue::ScalarOrList(ScalarOrList::Scalar(lit))) => {
            Some(HealthcheckTest::Shell(lit))
        }
        Some(FieldValue::ScalarOrList(ScalarOrList::List(items, list_span))) => {
            Some(HealthcheckTest::Exec(items, list_span))
        }
        _ => None,
    };
    let interval = match fields.remove("interval") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let timeout = match fields.remove("timeout") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let retries = match fields.remove("retries") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let start_period = match fields.remove("start_period") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let start_interval = match fields.remove("start_interval") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let disable = match fields.remove("disable") {
        Some(FieldValue::Flag(s)) => Some(s),
        _ => None,
    };
    Healthcheck {
        test,
        interval,
        timeout,
        retries,
        start_period,
        start_interval,
        disable,
        span,
    }
}
