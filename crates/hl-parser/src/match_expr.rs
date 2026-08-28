//! The `router { rule: ... }` expression grammar (#228).
//!
//! A rule is a boolean expression over Traefik's own rule matchers:
//!
//! ```text
//! rule_expr ::= or_expr
//! or_expr   ::= and_expr ( "||" and_expr )*
//! and_expr  ::= unary ( "&&" unary )*
//! unary     ::= "!" unary | primary
//! primary   ::= "(" rule_expr ")" | matcher
//! matcher   ::= IDENT "(" ( literal ( "," literal )* )? ")"
//! ```
//!
//! The precedence — `!` tightest, then `&&`, then `||` — is Traefik's
//! own, which is what lets [`hl_codegen`] render a parsed tree back out
//! without parenthesizing everything defensively: an expression written
//! without parentheses reparses in Traefik as the same tree it parsed as
//! here.
//!
//! # Where the expression ends
//!
//! The lexer emits no newline token, so nothing but lookahead separates
//! one statement from the next, and an expression-valued field has to be
//! careful about where it stops. This one stops as soon as the next
//! token is neither `&&` nor `||`, and that is unambiguous in all three
//! places a `rule` can be written:
//!
//! - In a braced `router { ... }` body, what follows is `}` or the next
//!   field's own key.
//! - In the comma-continued form — `router api, rule: Host("x"),
//!   entrypoint: web` — the expression consumes a `,` **only** between a
//!   matcher's own parentheses, never at the top level, so the comma
//!   after `Host("x")` is left for `parse_secondary_fields` exactly as
//!   it would be after any other field's value.
//! - A `(` is a group only in `primary` position and an argument list
//!   only immediately after a matcher's name. A `template`'s parameter
//!   list is the one other `(` in the grammar and is reachable only
//!   after the `template` keyword at the top level, so the two never
//!   compete for the same token.
//!
//! A bare `IDENT` not followed by `(` is an error rather than a fallback
//! to a plain literal: one token of lookahead settles it, and a silent
//! fallback would let `rule: web-secure` compile into a rule Traefik
//! rejects at load time instead of one `hllc` rejects at the spot it was
//! written.

use hl_lexer::{Span, TokenKind};

use crate::ast::{Ident, MatchExpr};
use crate::error::{Expected, ParseError};
use crate::matchers;
use crate::parser::Parser;

/// How deeply a `rule` expression may nest before
/// [`ParseError::MatchExprTooDeep`] stops it.
///
/// A rule expression is the language's *second* genuinely self-recursive
/// production, after `raw`'s schema-free value grammar, so it needs the
/// ceiling [`crate::MAX_RAW_VALUE_DEPTH`] documents in full: with no
/// limit, a few kilobytes of `((((...))))` overflows the stack, and a
/// stack overflow *aborts the process* rather than returning an error a
/// library embedder can catch (#72). Dropping the tree recurses through
/// drop glue as well, so the ceiling has to leave room for that too.
///
/// The depth counted here is the depth of the *tree*, not of the parser's
/// own call chain, which is why a long `a && b && c && ...` chain counts
/// against it: `&&` folds left, so each extra operand is one more level
/// of `Box<MatchExpr>` for drop glue to walk, exactly as an extra `(`
/// would be.
///
/// 128 matches `raw`'s limit and is picked the same way — far beyond any
/// real rule, which nests two or three deep at most, while staying well
/// under what a 2 MiB spawned-thread stack survives.
pub const MAX_MATCH_EXPR_DEPTH: usize = 128;

/// Spans the text from `start`'s beginning to `end`'s end, keeping
/// `start`'s line/column so a diagnostic points at where the node
/// begins.
fn joined(start: Span, end: Span) -> Span {
    Span {
        end: end.end,
        ..start
    }
}

impl Parser<'_> {
    /// Parses a `rule:` field's value — the optional leading colon, then
    /// the whole expression.
    ///
    /// The colon is optional here for the same reason it is for every
    /// other field kind (`parse_field_value_literal`,
    /// `parse_nested_into`): the language accepts `key: value` and the
    /// bare `key value` sugar interchangeably.
    pub(crate) fn parse_match_expr_value(&mut self) -> Result<MatchExpr, ParseError> {
        if self.peek().kind == TokenKind::Colon {
            self.bump();
        }
        self.parse_or_expr(0)
    }

    fn check_depth(&self, level: usize) -> Result<(), ParseError> {
        if level > MAX_MATCH_EXPR_DEPTH {
            return Err(ParseError::MatchExprTooDeep {
                limit: MAX_MATCH_EXPR_DEPTH,
                span: self.peek().span,
            });
        }
        Ok(())
    }

    fn parse_or_expr(&mut self, depth: usize) -> Result<MatchExpr, ParseError> {
        let mut node = self.parse_and_expr(depth)?;
        let mut level = depth;
        while self.peek().kind == TokenKind::PipePipe {
            level += 1;
            self.check_depth(level)?;
            self.bump();
            let rhs = self.parse_and_expr(level)?;
            let span = joined(node.span(), rhs.span());
            node = MatchExpr::Or {
                lhs: Box::new(node),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(node)
    }

    fn parse_and_expr(&mut self, depth: usize) -> Result<MatchExpr, ParseError> {
        let mut node = self.parse_unary(depth)?;
        let mut level = depth;
        while self.peek().kind == TokenKind::AmpAmp {
            level += 1;
            self.check_depth(level)?;
            self.bump();
            let rhs = self.parse_unary(level)?;
            let span = joined(node.span(), rhs.span());
            node = MatchExpr::And {
                lhs: Box::new(node),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(node)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<MatchExpr, ParseError> {
        if self.peek().kind == TokenKind::Bang {
            self.check_depth(depth + 1)?;
            let bang = self.bump();
            let operand = self.parse_unary(depth + 1)?;
            let span = joined(bang.span, operand.span());
            return Ok(MatchExpr::Not {
                operand: Box::new(operand),
                span,
            });
        }
        self.parse_primary(depth)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<MatchExpr, ParseError> {
        match self.peek().kind {
            TokenKind::LParen => {
                self.check_depth(depth + 1)?;
                let open = self.bump();
                let inner = self.parse_or_expr(depth + 1)?;
                let close = self.expect(TokenKind::RParen)?;
                Ok(MatchExpr::Group {
                    inner: Box::new(inner),
                    span: joined(open.span, close.span),
                })
            }
            TokenKind::Ident => self.parse_matcher(),
            _ => Err(self.unexpected(Expected::Description(
                "a rule matcher, `!`, or `(` — for example `Host(\"example.com\")`",
            ))),
        }
    }

    /// `IDENT "(" ( literal ( "," literal )* )? ")"`, checked against
    /// [`crate::matchers`] as it's built.
    ///
    /// Both checks land here rather than in codegen, and the split from
    /// `protocol` — which *is* checked in codegen, precisely so a
    /// template can write `protocol: $proto` — is principled rather than
    /// arbitrary. A matcher's name is an `IDENT` token, which no
    /// substitution can reach: `$param` is a [`crate::Literal`] form, and
    /// a matcher name isn't a literal. Its argument *count* is fixed at
    /// parse time for the same reason — substitution replaces one
    /// literal with one literal and never expands a list. Nothing
    /// composition does can change either answer, so checking here costs
    /// nothing and buys the span of the matcher as it was actually
    /// written.
    ///
    /// Which *namespace* a matcher is legal in does depend on
    /// `protocol`, so that check stays in codegen with the rest of the
    /// protocol-dependent ones.
    fn parse_matcher(&mut self) -> Result<MatchExpr, ParseError> {
        let name_tok = self.bump();
        let name = Ident {
            name: name_tok.lexeme.to_string(),
            span: name_tok.span,
        };
        if self.peek().kind != TokenKind::LParen {
            // Reported at the name, not at whatever token happens to
            // follow it: the mistake is that this name was written where
            // a matcher belongs, and the next token is often on a later
            // line and has nothing to do with it (#87's reasoning for
            // `MapEntryMissingSeparator`, one production over).
            return Err(ParseError::UnexpectedToken {
                expected: Expected::Description(
                    "a rule matcher — its arguments go in parentheses, as `Host(\"example.com\")`",
                ),
                found_kind: name_tok.kind,
                found_lexeme: name_tok.lexeme.to_string(),
                span: name.span,
            });
        }
        self.bump();
        let mut args = Vec::new();
        if self.peek().kind != TokenKind::RParen {
            loop {
                args.push(self.parse_literal_reference()?);
                if self.peek().kind == TokenKind::Comma {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let close = self.expect(TokenKind::RParen)?;

        let Some(matcher) = matchers::lookup(&name.name) else {
            return Err(ParseError::UnknownMatcher {
                name: name.name,
                span: name.span,
            });
        };
        if args.len() != matcher.arity {
            return Err(ParseError::MatcherArity {
                name: matcher.name,
                expected: matcher.arity,
                found: args.len(),
                span: joined(name.span, close.span),
            });
        }

        let span = joined(name.span, close.span);
        Ok(MatchExpr::Matcher { name, args, span })
    }
}
