use std::collections::HashMap;

use hl_lexer::{FileId, Lexer, Span, Token, TokenKind};

use crate::ast::{
    ArrowMap, ArrowMapEntry, ArrowMapHost, Build, Command, DependsOnCondition, DependsOnEntry,
    Entrypoint, EnvEntry, EnvMap, Expose, Healthcheck, HealthcheckTest, Ident, Image, Literal,
    MatchExpr, Network, Param, Program, QualifiedRef, RawEntry, RawMap, RawValue, Restart, Router,
    Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, Traefik, UseDecl, Volume,
    VolumeDriverOpt,
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

/// Builds the [`Literal::Str`] a `STRING` token stands for, decoding its
/// backslash escapes (#181).
///
/// The AST stores the *decoded* value — `\n` as a newline, `\"` as a
/// quote — because that's what every later stage means by a string's
/// content: interpolation scans it, codegen writes it into the Compose
/// document. The token's own `lexeme` stays the undecoded source text,
/// so the literal's [`Span`] keeps describing real source bytes even
/// once the decoded value is shorter than what was written.
///
/// The `Err` arm is unreachable through the parser's own entry points,
/// which tokenize first and never reach parsing with an invalid escape
/// in hand (see [`hl_lexer::unescape`], which the lexer runs as it
/// scans). It's mapped rather than unwrapped so that a `Parser` built
/// from tokens some other way still reports the error instead of
/// panicking.
fn str_literal(tok: Token<'_>) -> Result<Literal, ParseError> {
    let text =
        hl_lexer::unescape(tok.lexeme, tok.span).map_err(|err| ParseError::Lex(vec![err]))?;
    Ok(Literal::Str(text.into_owned(), tok.span))
}

/// One resolved field's accumulated value, keyed by field name inside a
/// struct-kind body. This is the "FieldMap" the generic engine builds up
/// before lowering it into a concrete AST struct once the body finishes.
enum FieldValue {
    Scalar(Literal),
    /// A `router`'s `rule` expression (#228) — single-occurrence like
    /// [`Self::Scalar`], since a router has one rule.
    Match(MatchExpr),
    /// Span of the bare flag token that set it.
    Flag(Span),
    /// A single-occurrence nested struct-kind field (image/expose/restart).
    Struct(StructFields, Span),
    /// An accumulating nested map-kind field (env/publish/driver_opts):
    /// (key, value, entry span).
    LiteralMap(Vec<(Literal, Literal, Span)>),
    /// An accumulating `volume` field: same shape as [`Self::LiteralMap`]
    /// except the key side is an [`ArrowMapHost`], which the parser has
    /// already split into a bind-mount literal or a named-volume
    /// reference (see [`schema::TypeSchema::key_may_be_reference`]), and
    /// a third `bool` carrying whether the entry's optional `{ read_only
    /// }` body (#158) was present. Not folded into [`Self::LiteralMap`]'s
    /// own `(Literal, Literal, Span)` shape even though `volume` is the
    /// only field parsed through this path, since `env`/`publish`/
    /// `driver_opts`/`devices` share that shape too and gain nothing from
    /// carrying a flag none of them has — `publish`/`devices` still lower
    /// into the very same [`crate::ast::ArrowMapEntry`] `volume` does
    /// (see `lower_service_fields`), just via [`Self::LiteralMap`]
    /// instead, since neither ever needs a named-reference host or a
    /// trailing modifier body at parse time.
    MountMap(Vec<(ArrowMapHost, Literal, bool, Span)>),
    /// An accumulating nested schema-free map-kind field (raw).
    Raw(RawMap),
    /// An accumulating reference-list field (middleware/networks/dns/
    /// env_file/router.entrypoint/router.path_prefix).
    /// See [`schema::FieldKind::ReferenceList`]'s doc for why one `Vec`
    /// of [`Literal`]s now covers every row, `path_prefix` included.
    RefList(Vec<Literal>),
    /// An accumulating template-invocation-list field (`with`'s `templates`).
    TemplateInvocations(Vec<TemplateInvocation>),
    /// A single-occurrence field whose value is either a bare literal or
    /// a bracketed list of literals (`healthcheck`'s `test` and
    /// `command`, #156). See [`schema::FieldKind::ScalarOrList`]'s doc.
    ScalarOrList(ScalarOrList),
    /// An accumulating `depends_on`-list field. See
    /// [`schema::FieldKind::DependsOnList`]'s doc.
    DependsOnEntries(Vec<DependsOnEntry>),
    /// An accumulating, name-keyed nested struct-kind field (`router`,
    /// #184): one `(optional name, its own field bag, its span)` per
    /// block written, in source order. See
    /// [`schema::FieldKind::NamedNested`]'s doc.
    NamedStructs(Vec<(Option<Ident>, StructFields, Span)>),
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
            FieldValue::Match(expr) => expr.span(),
            FieldValue::Flag(span) => *span,
            FieldValue::Struct(_, span) => *span,
            FieldValue::ScalarOrList(v) => v.span(),
            FieldValue::LiteralMap(_)
            | FieldValue::MountMap(_)
            | FieldValue::Raw(_)
            | FieldValue::RefList(_)
            | FieldValue::TemplateInvocations(_)
            | FieldValue::DependsOnEntries(_)
            | FieldValue::NamedStructs(_) => {
                unreachable!("map/list-kind fields accumulate and are never duplicate-checked")
            }
        }
    }
}

type StructFields = HashMap<&'static str, FieldValue>;

pub(crate) struct Parser<'src> {
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

    pub(crate) fn peek(&self) -> &Token<'src> {
        // `tokens` always ends with Eof (Lexer::tokenize guarantees this),
        // and `bump` refuses to advance past it, so this never runs off
        // the end.
        &self.tokens[self.pos]
    }

    pub(crate) fn bump(&mut self) -> Token<'src> {
        let tok = self.tokens[self.pos];
        if self.tokens[self.pos].kind != TokenKind::Eof {
            self.pos += 1;
        }
        tok
    }

    pub(crate) fn expect(&mut self, kind: TokenKind) -> Result<Token<'src>, ParseError> {
        let tok = *self.peek();
        if tok.kind == kind {
            self.bump();
            Ok(tok)
        } else {
            Err(self.unexpected(Expected::Token(kind)))
        }
    }

    pub(crate) fn unexpected(&self, expected: Expected) -> ParseError {
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
                str_literal(tok)
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
                str_literal(tok)
            }
            TokenKind::Ident => {
                self.bump();
                Ok(Literal::Ident(tok.lexeme.to_string(), tok.span))
            }
            _ => Err(self.unexpected(Expected::Description("a field name (identifier or string)"))),
        }
    }

    /// `reference ::= ( key ( "." IDENT )? ) | "$" IDENT` — every
    /// reference-shaped position's own value grammar (#196):
    /// `middleware`/`networks`/`dns`/`env_file`/`router.entrypoint`/
    /// `router.path_prefix`/`router.middleware`, a `depends_on` entry,
    /// and a named-volume mount's host side. The trailing `.IDENT` names
    /// an import alias's declaration (`traefik.traefik-net`); only a
    /// plain `IDENT` key can be qualified this way — a `STRING` key's
    /// content is just string content, never followed by a structural
    /// `.`. `NUMBER` is deliberately not part of this grammar (unlike
    /// [`Self::parse_literal`]'s): [`Self::parse_key`] underlies it just
    /// as it always has, since a number was never a legal
    /// network/middleware/service/entry-point name and nothing about
    /// unifying [`Literal`] and the old `Reference` type changes that.
    ///
    /// The `$` arm is what #196 actually adds: before it, this function
    /// (then named `parse_reference`) built a `Reference`, which had
    /// nowhere to put a `Param`, so `networks [$net]` was a parse error
    /// with no way to fix it short of a second grammar for this position.
    /// Whether `$param` is legal at all is [`Self::parse_param_reference`]'s
    /// own concern (a template body or not), asked here exactly as it's
    /// asked from [`Self::parse_literal`] — this function doesn't ask a
    /// second question of its own.
    ///
    /// Whether the *qualified* form parsed here is legal at the calling
    /// position is likewise not this function's concern: every
    /// reference-shaped position parses it uniformly, and
    /// `compose::reject_qualified` (driven by
    /// [`schema::allows_qualified_reference`]) rejects it afterward,
    /// post-parse, everywhere but `networks` and a named-volume host.
    pub(crate) fn parse_literal_reference(&mut self) -> Result<Literal, ParseError> {
        if self.peek().kind == TokenKind::Dollar {
            let dollar = *self.peek();
            return self.parse_param_reference(dollar);
        }
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
            return Ok(Literal::Qualified(Box::new(QualifiedRef {
                qualifier,
                name: name_tok.lexeme.to_string(),
                name_span: name_tok.span,
                span,
            })));
        }
        Ok(key)
    }

    fn parse_bracket_reference_list(&mut self) -> Result<Vec<Literal>, ParseError> {
        self.expect(TokenKind::LBracket)?;
        let mut refs = Vec::new();
        if self.peek().kind != TokenKind::RBracket {
            loop {
                refs.push(self.parse_literal_reference()?);
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
    /// the same reason now that `router.entrypoint` is a reference list:
    /// in `router api, entrypoint: web, host: "x"`, the second comma
    /// starts a sibling *field* of `router`, not a second entry point,
    /// and a greedy list would swallow `host` as one and then fail on
    /// its `:` with an error pointing nowhere near the real problem.
    ///
    /// `KEY :` is the whole tell: a list item is a bare reference
    /// (optionally `alias.name`, optionally `$param`), never a `key:
    /// value` pair, so a colon one token past the comma can only mean a
    /// new field has begun.
    fn parse_bare_reference_list(&mut self) -> Result<Vec<Literal>, ParseError> {
        let mut refs = vec![self.parse_literal_reference()?];
        while self.peek().kind == TokenKind::Comma && !self.comma_starts_a_new_field() {
            self.bump();
            refs.push(self.parse_literal_reference()?);
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

    /// A [`schema::FieldKind::ReferenceList`] value: an optional leading
    /// `:`, then either a bracketed list of references or the bare
    /// comma-list sugar. Covers every row that kind drives — see that
    /// kind's own doc.
    fn parse_reference_list_value(&mut self) -> Result<Vec<Literal>, ParseError> {
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
    /// `test`'s exec form, `["CMD", "pg_isready", "-U", "miniflux"]`, or
    /// `command`'s own exec form, `["npm", "start"]`, #156). Items go
    /// through [`Self::parse_literal`], not
    /// [`Self::parse_literal_reference`]: an entry here never resolves
    /// against anything an `.hll` file declares, so — unlike
    /// [`Self::parse_bracket_reference_list`] — it never carries an
    /// `alias.` qualifier either.
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
        let reference = self.parse_literal_reference()?;
        let reference_span = reference.span();
        let mut end = reference_span.end;
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
            start: reference_span.start,
            end,
            line: reference_span.line,
            col: reference_span.col,
            file: reference_span.file,
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
    /// e.g. `image "nginx:alpine"` instead of `image { ref:
    /// "nginx:alpine" }`. After the primary value, it continues
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

        // Beyond that, zero or more explicit secondary fields.
        self.parse_secondary_fields(nested, &mut fields)?;

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

    /// Zero or more explicit `, key: value` secondary fields of `nested`,
    /// continuing whatever value the caller has already started — the
    /// same "trailing comma continues, its absence ends the statement"
    /// rule every other comma-list in the grammar follows: a comma is
    /// required before each one, and one-token lookahead past it
    /// confirms the next key genuinely names one of the nested type's
    /// own fields before consuming it as part of this value — otherwise
    /// the comma (and whatever follows it) is left for the enclosing
    /// body, where a bare comma is never a valid statement start and so
    /// correctly errors instead of silently reattaching elsewhere.
    ///
    /// Factored out of [`Self::parse_struct_primary_shorthand`] when
    /// `router` gained the same tail (#184): `router api, host: "...",
    /// entrypoint: web-secure` continues from a *name* rather than from
    /// a primary value, but everything after that first token is the
    /// identical production, and having one copy of it is what keeps the
    /// two spellings from drifting.
    fn parse_secondary_fields(
        &mut self,
        nested: &'static TypeSchema,
        fields: &mut StructFields,
    ) -> Result<(), ParseError> {
        loop {
            if self.peek().kind != TokenKind::Comma {
                break;
            }
            let lookahead = &self.tokens[self.pos + 1];
            let continues = match lookahead.kind {
                // Only a real field of `nested` continues the list — a
                // name that merely *used* to be one (`FieldResolution::
                // Moved`) is no more a continuation than an unknown key
                // is, and letting it continue would report the move
                // against the wrong body.
                TokenKind::Ident | TokenKind::Str => matches!(
                    schema::resolve_field(nested, lookahead.lexeme),
                    FieldResolution::Field(_)
                ),
                _ => false,
            };
            if !continues {
                break;
            }
            self.bump();
            self.parse_statement_into(nested, fields)?;
        }
        Ok(())
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
            FieldResolution::Moved(guidance) => {
                return Err(ParseError::MovedField {
                    type_name: schema.type_name,
                    field: key_text,
                    guidance,
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
            FieldKind::NamedNested(nested) => {
                self.parse_named_nested_into(field, nested, key_span, fields)
            }
            FieldKind::MatchExpr => {
                let expr = self.parse_match_expr_value()?;
                self.insert_single(schema, fields, field.name, FieldValue::Match(expr))
            }
        }
    }

    /// Parses one `router <name>? ( "{" body "}" | ( "," field )* )`
    /// statement (#184) and appends it to the field's accumulated list.
    ///
    /// Two spellings, mirroring what the rest of the grammar already
    /// offers a struct-kind value:
    ///
    /// - the canonical braced body, `router api { host: "..." }`, whose
    ///   fields are newline-separated like any other struct body;
    /// - the comma-continued form, `router api, host: "...", entrypoint:
    ///   web-secure`, which reuses [`Self::parse_secondary_fields`]
    ///   verbatim — the same production `image "ref", ...` (or any other
    ///   primary-shorthand nested type with more than one field) already
    ///   parses, continuing from the router's name instead of from a
    ///   primary value.
    ///
    /// The name is optional and, when present, is an `IDENT` — never a
    /// `STRING`, and never a `$param`. It lands in a label *key*, so the
    /// narrow grammar is the first half of the injection guard (see
    /// [`crate::ast::Router`]); codegen's own check is the second.
    /// Leaving the name off (`router { ... }`) is the unnamed form,
    /// claiming the service's own router id — writing it twice in one
    /// body (whether by hand or via `expose <port> as "<host>"`'s own
    /// sugar) is [`ParseError::DuplicateRouterName`].
    ///
    /// The unnamed form requires the braced body: with no name and no
    /// `{`, there is no first token to continue a comma-list from, and
    /// `router host: "x"` would have to guess whether `host` names the
    /// router or its own field. Guessing is what the braces exist to
    /// avoid.
    fn parse_named_nested_into(
        &mut self,
        field: &'static FieldSchema,
        nested: &'static TypeSchema,
        key_span: Span,
        fields: &mut StructFields,
    ) -> Result<(), ParseError> {
        // Mirrors `parse_nested_into`: an optional leading colon
        // (`router: { ... }`) is accepted alongside the bare form.
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        let name = if self.peek().kind == TokenKind::Ident {
            let tok = self.bump();
            Some(Ident {
                name: tok.lexeme.to_string(),
                span: tok.span,
            })
        } else {
            None
        };
        let (nested_fields, span) = if self.peek().kind == TokenKind::LBrace {
            let (nested_fields, body_span) = self.parse_struct_body(nested)?;
            let span = join_spans(key_span, body_span);
            (nested_fields, span)
        } else if name.is_some() {
            let mut nested_fields = StructFields::new();
            self.parse_secondary_fields(nested, &mut nested_fields)?;
            let last_end = self.tokens[self.pos.saturating_sub(1)].span.end;
            let span = Span {
                end: last_end,
                ..key_span
            };
            (nested_fields, span)
        } else {
            return Err(self.unexpected(Expected::Description("a name or `{`")));
        };
        match fields
            .entry(field.name)
            .or_insert_with(|| FieldValue::NamedStructs(Vec::new()))
        {
            FieldValue::NamedStructs(v) => v.push((name, nested_fields, span)),
            _ => unreachable!("field kind is stable for a given field name"),
        }
        Ok(())
    }

    /// `expose <port> as "<host>"` (#198): desugars to `expose { port }`
    /// plus an unnamed `router { host }`. Called by
    /// [`Self::parse_nested_into`] right after the `expose` field's own
    /// primary-value shorthand has already parsed `port`, with the `as`
    /// keyword still unconsumed.
    ///
    /// Bespoke grammar, not schema-driven sugar: through #197 this same
    /// spelling was `TypeSchema::bare_keyword_alias`, a generic
    /// keyword→field alias that fused `as` onto `EXPOSE`'s own `host`
    /// field. #198 moved every Traefik-routing field off `expose` and
    /// onto `router`, so `EXPOSE` has no `host` left for a generic alias
    /// to target — but the spelling itself is kept, verbatim, because
    /// it's the shortest way to write the overwhelmingly common
    /// single-router service and it's all over the book (F6 of #198). An
    /// unnamed `router` is exactly what an unadorned "give this service a
    /// host" needs, so that's what this pushes onto the enclosing body's
    /// own `router` field — the very entry a hand-written `router { host:
    /// "..." }` block would have produced.
    ///
    /// Deliberately a dead end, exactly as the old alias sugar was: `as`
    /// fuses onto the primary value as one self-contained unit (docs/
    /// DESIGN.md's desugaring rule 3) and can't itself be followed by
    /// further secondary fields, comma or no comma — a service that needs
    /// more than a bare host must write the router out explicitly
    /// (`expose <port>` plus `router { host: "...", entrypoint: ... }`).
    /// Unlike before #198 (`ParseError::AliasSugarCannotContinue`, see
    /// F6), there is no dedicated diagnostic for a trailing comma here:
    /// whatever follows is left for the enclosing body's own statement
    /// loop, which reports whatever generic error a stray token there
    /// produces — the same "expected a newline before the next field"
    /// message #87 originally motivated a dedicated error to avoid, now
    /// accepted as the cost of not carrying a schema mechanism forward
    /// for the sake of one spelling.
    ///
    /// A service that writes both this sugar and an explicit unnamed
    /// `router { ... }` block gets [`ParseError::DuplicateRouterName`] —
    /// the two pushed entries share the same `None` key — rather than a
    /// dedicated diagnostic of its own (the old
    /// `CodegenError::ExposeHostWithUnnamedRouter` is gone for the same
    /// reason; see `hl_codegen`'s own doc).
    fn parse_expose_as_sugar(&mut self, fields: &mut StructFields) -> Result<(), ParseError> {
        let as_span = self.bump().span; // the `as` keyword itself
        let host = self.parse_literal()?;
        let span = Span {
            end: host.span().end,
            ..as_span
        };
        let mut router_fields = StructFields::new();
        router_fields.insert("host", FieldValue::Scalar(host));
        match fields
            .entry("router")
            .or_insert_with(|| FieldValue::NamedStructs(Vec::new()))
        {
            FieldValue::NamedStructs(v) => v.push((None, router_fields, span)),
            _ => unreachable!("field kind is stable for a given field name"),
        }
        Ok(())
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
                //
                // No `!= LBrace` guard here: `at_value_start` already
                // excludes `{`, so pairing the two produced a mutant no
                // input could kill — the two spellings agree on every
                // reachable path.
                let took_primary_shorthand = self.at_value_start();
                let (nested_fields, span) = if self.peek().kind == TokenKind::LBrace {
                    self.parse_struct_body(nested)?
                } else if nested.primary_field.is_some() && self.at_value_start() {
                    self.parse_struct_primary_shorthand(nested)?
                } else if nested.primary_field.is_none() {
                    return Err(self.unexpected(Expected::Token(TokenKind::LBrace)));
                } else {
                    return Err(self.unexpected(Expected::Description("a value or `{`")));
                };
                // `expose <port> as "<host>"` (#198): bespoke sugar, only
                // reachable right after the bare primary-value shorthand —
                // never after a braced `expose { ... }` body, matching
                // where the old schema-driven `as`→`host` alias lived
                // before F6 removed it — see
                // [`Self::parse_expose_as_sugar`]'s own doc.
                if took_primary_shorthand
                    && std::ptr::eq(nested, &schema::EXPOSE)
                    && self.peek().kind == TokenKind::Ident
                    && self.peek().lexeme == "as"
                {
                    self.parse_expose_as_sugar(fields)?;
                }
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
                    FieldValue::Raw(existing) => {
                        merge_raw_entries(&mut existing.entries, raw_map.entries)?;
                    }
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
                merge_mount_map_entries(nested, bucket, entries)
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
    /// [`Self::parse_literal_map_entry`], except the host side is an
    /// [`ArrowMapHost`]: a quoted string (or other literal) is a
    /// bind-mount path, and a bare `IDENT` is a reference to a top-level
    /// `volume` declaration, optionally `alias.`-qualified. See
    /// [`ArrowMapHost`]'s own doc for why that distinction is drawn here,
    /// at parse time, rather than from the string's shape later on.
    /// Reached only for `volume`, the one field whose schema sets
    /// [`schema::TypeSchema::key_may_be_reference`] — `publish`/`devices`
    /// entries go through [`Self::parse_literal_map_entry`] instead,
    /// whose plain [`Literal`] host [`lower_service_fields`] wraps in
    /// [`ArrowMapHost::BindMount`] on the way into the very same
    /// [`crate::ast::ArrowMapEntry`] this function builds.
    ///
    /// The container literal may be followed by an optional `{ read_only
    /// }` body (#158), shaped like [`Self::parse_depends_on_entry`]'s own
    /// trailing `{ condition: ... }`: an `IDENT` (bare presence only, no
    /// `:`/value, matching [`schema::FieldKind::BoolFlag`]'s own
    /// convention for `external`/`disable`) that must read literally
    /// `read_only`, then `}`. See [`crate::ast::ArrowMapEntry`]'s own doc
    /// for why this shape was chosen over the issue's other candidates,
    /// and for why only this path — never `publish`/`devices`' own — ever
    /// produces one.
    fn parse_mount_map_entry(
        &mut self,
        schema: &'static TypeSchema,
    ) -> Result<(ArrowMapHost, Literal, bool, Span), ParseError> {
        let host = if self.peek().kind == TokenKind::Ident {
            ArrowMapHost::Named(self.parse_literal_reference()?)
        } else {
            ArrowMapHost::BindMount(self.parse_literal()?)
        };
        let span = host.span();
        self.expect_map_separator(schema, span)?;
        let container = self.parse_literal()?;
        let mut entry_span = join_spans(span, container.span());
        let read_only = if self.peek().kind == TokenKind::LBrace {
            self.bump();
            let key_tok = self.expect(TokenKind::Ident)?;
            if key_tok.lexeme != "read_only" {
                return Err(ParseError::UnknownField {
                    type_name: schema.type_name,
                    field: key_tok.lexeme.to_string(),
                    raw_escape_hatch: false,
                    span: key_tok.span,
                });
            }
            let close = self.expect(TokenKind::RBrace)?;
            entry_span = join_spans(span, close.span);
            true
        } else {
            false
        };
        Ok((host, container, read_only, entry_span))
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
                let mut entries: Vec<(Literal, RawValue)> = Vec::new();
                while self.peek().kind != TokenKind::RBrace {
                    let key = self.parse_key()?;
                    self.expect(TokenKind::Colon)?;
                    let value = self.parse_raw_value(depth + 1)?;
                    // This mapping's own keys only — never the enclosing
                    // one's, and never a mapping nested inside `value`,
                    // which checked itself on the way out of its own
                    // recursive call. That is what keeps `raw { a: { x: 1
                    // }, b: { x: 2 } }` legal while rejecting a single
                    // mapping that names `x` twice (#206).
                    reject_duplicate_raw_key(
                        entries.iter().map(|(held, _)| (held.text(), held.span())),
                        key.text(),
                        key.span(),
                    )?;
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
            )?))),
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
        let path = str_literal(path_tok)?;
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

    /// `template_decl ::= "template" IDENT param_list? body`.
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
        let (fields_map, body_span) = if self.peek().kind == TokenKind::LBrace {
            self.parse_struct_body(&schema::TEMPLATE)?
        } else {
            return Err(self.unexpected(Expected::Description("`{`")));
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
            fields: lower_service_fields(fields_map)?,
            span,
        })
    }

    /// `param_list ::= "(" ( param ( "," param )* )? ")"`,
    /// `param ::= IDENT`. #201 dropped the `: Number|String` annotation a
    /// `param` used to optionally carry — see [`crate::ast::Param`]'s own
    /// doc for why — so a `:` right after a parameter name is no longer
    /// the start of anything this parser recognizes: the loop below falls
    /// through to `self.expect(TokenKind::RParen)` and reports the `:` as
    /// the unexpected token, the same generic diagnostic any other
    /// unrecognized token in this position would get.
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
                params.push(Param { name });
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
/// map-kind field: `env`/`publish`/`driver_opts`/`devices` key on a
/// [`Literal`], `volume` on an [`ArrowMapHost`]. `key_text` reads that
/// side's text — `Literal::text` or `ArrowMapHost::text`, passed by the
/// caller, since there is no one trait both already implement.
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

/// [`merge_map_entries`]'s own twin for `raw` — the one map-kind field
/// whose values are [`RawValue`] trees rather than [`Literal`]s, so it
/// can't ride that function's `(K, Literal, Span)` shape.
///
/// Before #206 this path simply concatenated, which made `raw` the last
/// map field where a key repeated inside one body silently lost a value:
/// `raw { user: "1000", user: "2000" }` emitted `user: '2000'` and said
/// nothing, while the same shape on `env` named both spans. The check
/// here is deliberately the same one [`merge_map_entries`] applies —
/// same [`ParseError::DuplicateMapKey`], same schema-declared uniqueness
/// side, first occurrence keeps the blame span — so the two diagnostics
/// read alike.
///
/// `bucket` is everything the enclosing body has accumulated for this
/// field so far, which is what makes two `raw { }` blocks in one
/// `service`/`template` collide exactly as two `env` statements already
/// do. It stops there: a *nested* map inside a `raw` value is its own
/// mapping, checked on its own by [`reject_duplicate_raw_key`]'s other
/// caller.
fn merge_raw_entries(
    bucket: &mut Vec<RawEntry>,
    new_entries: Vec<RawEntry>,
) -> Result<(), ParseError> {
    for entry in new_entries {
        reject_duplicate_raw_key(
            bucket.iter().map(|held| (held.key.text(), held.span)),
            entry.key.text(),
            entry.span,
        )?;
        bucket.push(entry);
    }
    Ok(())
}

/// Rejects `key` when one of `held` — the keys of the *same* mapping,
/// each with the span to blame for its first occurrence — already claims
/// it.
///
/// Scoping the check to one mapping is the whole of #206's "worth
/// deciding first": `raw` is schema-free and its values recurse, so
/// "duplicate key" has to mean what YAML means by it, which is a
/// property of a single mapping and of nothing outside it. `raw { a: {
/// x: 1 }, b: { x: 2 } }` is two distinct mappings that each happen to
/// hold an `x`, and stays legal; only a mapping that names `x` twice
/// itself is an error.
///
/// The side and the type name come from [`schema::RAW`] rather than
/// being written out here, so this diagnostic and the cross-tier one
/// [`merge_map_entries`] raises can't drift apart. Only the key side is
/// meaningful for `raw` in any case — its values are trees, not
/// literals, so there's nothing on the value side to compare.
fn reject_duplicate_raw_key<'a>(
    held: impl IntoIterator<Item = (&'a str, Span)>,
    key: &str,
    span: Span,
) -> Result<(), ParseError> {
    let side = schema::RAW
        .uniqueness
        .expect("the raw schema must define a uniqueness side");
    for (existing, first) in held {
        if existing == key {
            return Err(ParseError::DuplicateMapKey {
                type_name: schema::RAW.type_name,
                side,
                value: key.to_string(),
                first,
                second: span,
            });
        }
    }
    Ok(())
}

/// [`merge_map_entries`]'s own twin for `volume`, the one map-kind field
/// whose entries carry a fourth element (the `{ read_only }` flag, #158)
/// alongside `(host, container, span)`. Not folded into
/// `merge_map_entries` itself by making that function generic over a
/// fourth type parameter: every other map-kind field (`env`/`publish`/
/// `driver_opts`) would then have to thread a `()` payload through calls
/// that have nothing to carry, for a genericity only one field ever uses.
/// The duplicate-checking logic is intentionally identical to
/// `merge_map_entries`'s own — same uniqueness side, same error — since
/// the flag is never part of what makes two entries collide, only extra
/// data riding along with whichever entry wins.
fn merge_mount_map_entries(
    nested: &'static TypeSchema,
    bucket: &mut Vec<(ArrowMapHost, Literal, bool, Span)>,
    new_entries: Vec<(ArrowMapHost, Literal, bool, Span)>,
) -> Result<(), ParseError> {
    let side = nested
        .uniqueness
        .expect("volume schema must define a uniqueness side");
    for (host, container, read_only, span) in new_entries {
        let check = match side {
            MapSide::Key => ArrowMapHost::text(&host),
            MapSide::Value => container.text(),
        }
        .to_string();
        let dup = bucket.iter().find(|(h, c, _, _)| {
            let existing = match side {
                MapSide::Key => ArrowMapHost::text(h),
                MapSide::Value => c.text(),
            };
            existing == check
        });
        if let Some((_, _, _, first_span)) = dup {
            return Err(ParseError::DuplicateMapKey {
                type_name: nested.type_name,
                side,
                value: check,
                first: *first_span,
                second: span,
            });
        }
        bucket.push((host, container, read_only, span));
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
///
/// Fallible only for `router` (#184): two blocks in one body claiming the
/// same router id would silently collapse to one, so the duplicate is
/// caught here, where both spans are still in hand — the same place and
/// the same reasoning as `merge_map_entries`' own intra-body uniqueness
/// check for `volume`/`env`/`publish`.
fn lower_service_fields(mut fields: StructFields) -> Result<ServiceFields, ParseError> {
    let image = match fields.remove("image") {
        Some(FieldValue::Struct(f, s)) => Some(lower_image(f, s)),
        _ => None,
    };
    let build = match fields.remove("build") {
        Some(FieldValue::Struct(f, s)) => Some(lower_build(f, s)),
        _ => None,
    };
    let expose = match fields.remove("expose") {
        Some(FieldValue::Struct(f, s)) => Some(lower_expose(f, s)),
        _ => None,
    };
    let routers = match fields.remove("router") {
        Some(FieldValue::NamedStructs(v)) => lower_routers(v)?,
        _ => Vec::new(),
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
    let command = match fields.remove("command") {
        Some(FieldValue::ScalarOrList(ScalarOrList::Scalar(lit))) => Some(Command::Shell(lit)),
        Some(FieldValue::ScalarOrList(ScalarOrList::List(items, list_span))) => {
            Some(Command::Exec(items, list_span))
        }
        _ => None,
    };
    // `entrypoint` (#183) lowers exactly like `command` just above —
    // the same `FieldKind::ScalarOrList` shape, into its own AST type.
    // Reached only for `entrypoint` written directly in a
    // `service`/`template` body: `router`'s own `entrypoint` sub-field
    // is an unrelated reference list, lowered by `lower_router` instead,
    // from a `StructFields` map the `router` schema built.
    let entrypoint = match fields.remove("entrypoint") {
        Some(FieldValue::ScalarOrList(ScalarOrList::Scalar(lit))) => Some(Entrypoint::Shell(lit)),
        Some(FieldValue::ScalarOrList(ScalarOrList::List(items, list_span))) => {
            Some(Entrypoint::Exec(items, list_span))
        }
        _ => None,
    };
    // `publish`'s entries are always [`ArrowMapHost::BindMount`] — its
    // schema leaves `key_may_be_reference` unset, so they're parsed via
    // [`FieldValue::LiteralMap`] as plain `(Literal, Literal, Span)`
    // pairs, not [`Self::parse_mount_map_entry`] — but they lower into
    // the very same [`crate::ast::ArrowMapEntry`] `volume` does, with
    // `read_only` always `false` (`publish` has no `{ read_only }`
    // syntax; a protocol suffix rides inside the container literal
    // instead — see [`crate::ast::ArrowMapEntry`]'s own doc).
    let publish = match fields.remove("publish") {
        Some(FieldValue::LiteralMap(entries)) => ArrowMap {
            entries: entries
                .into_iter()
                .map(|(host, container, span)| ArrowMapEntry {
                    host: ArrowMapHost::BindMount(host),
                    container,
                    read_only: false,
                    span,
                })
                .collect(),
        },
        _ => ArrowMap::default(),
    };
    let volumes = match fields.remove("volume") {
        Some(FieldValue::MountMap(entries)) => ArrowMap {
            entries: entries
                .into_iter()
                .map(|(host, container, read_only, span)| ArrowMapEntry {
                    host,
                    container,
                    read_only,
                    span,
                })
                .collect(),
        },
        _ => ArrowMap::default(),
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
    let privileged = match fields.remove("privileged") {
        Some(FieldValue::Flag(s)) => Some(s),
        _ => None,
    };
    // Same shape as `publish` above — always `BindMount`, never a
    // trailing `{ read_only }` body — see that field's own comment.
    let devices = match fields.remove("devices") {
        Some(FieldValue::LiteralMap(entries)) => ArrowMap {
            entries: entries
                .into_iter()
                .map(|(host, container, span)| ArrowMapEntry {
                    host: ArrowMapHost::BindMount(host),
                    container,
                    read_only: false,
                    span,
                })
                .collect(),
        },
        _ => ArrowMap::default(),
    };
    let with = match fields.remove("with") {
        Some(FieldValue::Struct(mut with_fields, _)) => match with_fields.remove("templates") {
            Some(FieldValue::TemplateInvocations(v)) => v,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    Ok(ServiceFields {
        image,
        build,
        expose,
        routers,
        traefik,
        restart,
        healthcheck,
        publish,
        volumes,
        env,
        raw,
        depends_on,
        networks,
        dns,
        env_file,
        privileged,
        devices,
        container_name,
        command,
        entrypoint,
        with,
    })
}

/// Lowers each accumulated `router` block into a [`Router`], rejecting
/// two blocks in one body that claim the same router id (#184).
///
/// A router id is what the emitted label key is built from
/// (`traefik.http.routers.<service>-<name>`), so two blocks sharing one
/// name aren't two routers — they're one router described twice, with
/// whichever came last silently winning. That is the same failure
/// `DuplicateMapKey` already refuses for two `volume` entries at one
/// container path, so it gets the same treatment: a hard error naming
/// both locations. The unnamed `router { }` form has an id too (the
/// service's own name), so writing it twice collides in exactly the same
/// way.
///
/// Note this is a *within one body* check. Two different tiers — a
/// template and the service that uses it — naming the same router is not
/// a duplicate at all but the merge this field is designed around; see
/// `compose.rs`'s `merge_routers`.
fn lower_routers(
    entries: Vec<(Option<Ident>, StructFields, Span)>,
) -> Result<Vec<Router>, ParseError> {
    let mut routers: Vec<Router> = Vec::with_capacity(entries.len());
    for (name, fields, span) in entries {
        let key = name.as_ref().map(|n| n.name.as_str());
        if let Some(first) = routers.iter().find(|r| r.key() == key) {
            return Err(ParseError::DuplicateRouterName {
                name: key.map(str::to_string),
                first: first.span,
                second: span,
            });
        }
        routers.push(lower_router(name, fields, span));
    }
    Ok(routers)
}

fn lower_router(name: Option<Ident>, mut fields: StructFields, span: Span) -> Router {
    let host = match fields.remove("host") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let entrypoint = match fields.remove("entrypoint") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    let path_prefix = match fields.remove("path_prefix") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    // #221: the router's own middleware list, read from the `router`
    // schema's own `middleware` row — not the service-level field of the
    // same name, which `lower_service_fields` reads from its own map.
    let middleware = match fields.remove("middleware") {
        Some(FieldValue::RefList(v)) => v,
        _ => Vec::new(),
    };
    // #225's three scalars, all plain `Scalar` rows like `host`.
    let priority = match fields.remove("priority") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let port = match fields.remove("port") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let protocol = match fields.remove("protocol") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    // #228's whole-rule spelling, the one field whose value is an
    // expression rather than a literal.
    let rule = match fields.remove("rule") {
        Some(FieldValue::Match(expr)) => Some(expr),
        _ => None,
    };
    Router {
        name,
        host,
        entrypoint,
        path_prefix,
        middleware,
        priority,
        port,
        protocol,
        rule,
        span,
    }
}

fn lower_service(name: Ident, fields: StructFields, span: Span) -> Result<Service, ParseError> {
    Ok(Service {
        name,
        fields: lower_service_fields(fields)?,
        span,
    })
}

fn lower_image(mut fields: StructFields, span: Span) -> Image {
    let reference = match fields.remove("ref") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Image { reference, span }
}

fn lower_build(mut fields: StructFields, span: Span) -> Build {
    let context = match fields.remove("context") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    let dockerfile = match fields.remove("dockerfile") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Build {
        context,
        dockerfile,
        span,
    }
}

fn lower_expose(mut fields: StructFields, span: Span) -> Expose {
    let port = match fields.remove("port") {
        Some(FieldValue::Scalar(lit)) => Some(lit),
        _ => None,
    };
    Expose { port, span }
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
