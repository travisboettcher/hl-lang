use std::iter::Peekable;
use std::str::CharIndices;

use crate::error::LexError;
use crate::span::{FileId, Span};
use crate::token::{Token, TokenKind};

/// Narrows a byte offset into a [`Span`]'s `u32` field, saturating
/// rather than wrapping (see [`Span`]'s "Why `u32` offsets"). A source
/// file over 4 GiB would have to exist for this to do anything at all;
/// if one ever did, a clamped offset is a wrong-but-bounded position
/// rather than one that has wrapped around to point at the start of the
/// file.
fn offset(byte: usize) -> u32 {
    u32::try_from(byte).unwrap_or(u32::MAX)
}

/// Scans hl-lang source text into a stream of [`Token`]s.
///
/// `Lexer` implements [`Iterator`] over `Result<Token<'src>, LexError>`.
/// The iterator yields a final [`TokenKind::Eof`] token exactly once and
/// then ends (`None`); if a [`LexError`] occurs, that error is yielded
/// once and the iterator ends immediately after — there is no error
/// recovery in this milestone, scanning simply stops.
///
/// # Example
///
/// ```
/// use hl_lexer::{Lexer, TokenKind};
///
/// let mut lexer = Lexer::new(r#"image "jellyfin/jellyfin:latest""#);
/// assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Ident);
/// assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Str);
/// assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Eof);
/// ```
pub struct Lexer<'src> {
    source: &'src str,
    chars: Peekable<CharIndices<'src>>,
    line: u32,
    col: u32,
    done: bool,
    /// Stamped into every [`Span`] this lexer produces, so positions
    /// stay attributable once more than one file is in play.
    file: FileId,
}

impl<'src> Lexer<'src> {
    /// Creates a new lexer over `source`, whose spans carry no file
    /// identity ([`FileId::ANONYMOUS`]).
    pub fn new(source: &'src str) -> Self {
        Lexer::new_in_file(source, FileId::ANONYMOUS)
    }

    /// Creates a new lexer over `source`, stamping `file` into every
    /// span it produces.
    ///
    /// `file` comes from a [`crate::SourceMap`] the caller owns — see
    /// `hl_linker`'s module graph, which interns each module's path as
    /// it loads it so a diagnostic can name the file a span belongs to
    /// even after composition has merged declarations from several of
    /// them into one service.
    pub fn new_in_file(source: &'src str, file: FileId) -> Self {
        Lexer {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            col: 1,
            done: false,
            file,
        }
    }

    /// Scans `source` in full, returning every token up to and including
    /// `Eof`, or the first [`LexError`] encountered.
    pub fn tokenize(source: &'src str) -> Result<Vec<Token<'src>>, LexError> {
        Lexer::new(source).collect()
    }

    /// Scans `source` in full, collecting *every* [`LexError`] encountered
    /// along the way rather than stopping at the first (#87) — each
    /// erroring call to [`Self::next_token`] has already consumed the
    /// offending character before returning `Err`, so scanning can simply
    /// continue from there without any extra recovery logic. Returns
    /// `Ok` only if zero errors occurred; otherwise every error found,
    /// in source order.
    ///
    /// Deliberately bypasses the [`Iterator`] impl, which fuses (stops
    /// for good) after the first `Err` — that behavior is preserved for
    /// existing callers of the iterator/[`Self::tokenize`] directly, and
    /// this is an additive alternative for a caller that wants every lex
    /// error at once instead.
    pub fn tokenize_collecting_errors(
        source: &'src str,
    ) -> Result<Vec<Token<'src>>, Vec<LexError>> {
        Lexer::tokenize_collecting_errors_in_file(source, FileId::ANONYMOUS)
    }

    /// [`Self::tokenize_collecting_errors`], stamping `file` into every
    /// span produced — the entry point `hl_parser::parse_in_file` (and
    /// through it the linker) uses.
    pub fn tokenize_collecting_errors_in_file(
        source: &'src str,
        file: FileId,
    ) -> Result<Vec<Token<'src>>, Vec<LexError>> {
        let mut lexer = Lexer::new_in_file(source, file);
        let mut tokens = Vec::new();
        let mut errors = Vec::new();
        loop {
            match lexer.next_token() {
                Ok(tok) => {
                    let is_eof = tok.kind == crate::token::TokenKind::Eof;
                    tokens.push(tok);
                    if is_eof {
                        break;
                    }
                }
                Err(err) => errors.push(err),
            }
        }
        if errors.is_empty() {
            Ok(tokens)
        } else {
            Err(errors)
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    /// Consumes and returns the next `(byte_offset, char)`, updating the
    /// running line/col position. A `\n` bumps `line` and resets `col` to
    /// 1; any other character (including `\r`, which never appears alone
    /// as a token) just bumps `col` — so a `\r\n` pair increments `line`
    /// exactly once, matching LF-only input.
    ///
    /// **Termination invariant**: this is the *only* place `self.chars`
    /// is ever advanced. Every unbounded loop in this module (this
    /// function's own callers in [`Self::skip_trivia`] and
    /// [`Self::next_token`]'s identifier/number scanning) relies on each
    /// iteration eventually calling `bump` to make real progress through
    /// `self.chars`, so the loop is bounded by `source`'s finite length.
    /// A mutation that makes `bump` stop actually consuming from
    /// `self.chars` (e.g. always returning `None`, or a fixed value
    /// without calling `self.chars.next()`) breaks that invariant and
    /// hangs those callers forever on any input reaching the affected
    /// loop — `cargo mutants` reports exactly that as a timeout rather
    /// than a normal caught/missed mutant; see
    /// `comment_at_eof_no_trailing_newline`/`comment_only_file` in
    /// `tests/lexer_tests.rs` for the adversarial inputs that exercise
    /// this loop all the way to a real `Eof`.
    fn bump(&mut self) -> Option<(usize, char)> {
        let next = self.chars.next();
        if let Some((_, c)) = next {
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
        next
    }

    /// Skips whitespace and `#`-to-end-of-line comments. Neither produces
    /// a token.
    ///
    /// Terminates because every branch that stays in a loop (the `#`
    /// comment body, and the outer `loop` re-checking after it) also
    /// calls [`Self::bump`], which strictly advances `self.chars` — see
    /// `bump`'s own doc for the invariant this depends on and what a
    /// mutation here (e.g. flipping the comment loop's `c == '\n'` check)
    /// does to it: `c == '\n'` breaking on the *first* comment char
    /// (`'#'` itself) instead of the real newline leaves `'#'`
    /// unconsumed, so the outer `loop` immediately re-matches `Some('#')`
    /// and repeats without ever calling `bump` — an infinite loop on any
    /// `#` comment, caught by `cargo mutants` as a timeout.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek_char() {
                Some('#') => {
                    while let Some(c) = self.peek_char() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                _ => break,
            }
        }
    }

    /// Scans and returns the next token, or the first error encountered.
    pub fn next_token(&mut self) -> Result<Token<'src>, LexError> {
        self.skip_trivia();

        let line = self.line;
        let col = self.col;

        let (start, ch) = match self.bump() {
            Some(pair) => pair,
            None => {
                let pos = self.source.len();
                return Ok(Token {
                    kind: TokenKind::Eof,
                    lexeme: &self.source[pos..pos],
                    span: Span {
                        start: offset(pos),
                        end: offset(pos),
                        line,
                        col,
                        file: self.file,
                    },
                });
            }
        };
        let first_end = start + ch.len_utf8();

        macro_rules! single {
            ($kind:expr) => {{
                Ok(Token {
                    kind: $kind,
                    lexeme: &self.source[start..first_end],
                    span: Span {
                        start: offset(start),
                        end: offset(first_end),
                        line,
                        col,
                        file: self.file,
                    },
                })
            }};
        }

        match ch {
            '{' => single!(TokenKind::LBrace),
            '}' => single!(TokenKind::RBrace),
            '[' => single!(TokenKind::LBracket),
            ']' => single!(TokenKind::RBracket),
            '(' => single!(TokenKind::LParen),
            ')' => single!(TokenKind::RParen),
            ':' => single!(TokenKind::Colon),
            ',' => single!(TokenKind::Comma),
            '=' => single!(TokenKind::Equals),
            '.' => single!(TokenKind::Dot),
            '$' => single!(TokenKind::Dollar),
            '-' => {
                if self.peek_char() == Some('>') {
                    let (idx, c) = self.bump().expect("peeked '>' must be consumable");
                    let end = idx + c.len_utf8();
                    Ok(Token {
                        kind: TokenKind::Arrow,
                        lexeme: &self.source[start..end],
                        span: Span {
                            start: offset(start),
                            end: offset(end),
                            line,
                            col,
                            file: self.file,
                        },
                    })
                } else {
                    Err(LexError::DanglingDash {
                        span: Span {
                            start: offset(start),
                            end: offset(first_end),
                            line,
                            col,
                            file: self.file,
                        },
                    })
                }
            }
            '"' => self.scan_string(start, line, col),
            c if c.is_ascii_digit() => Ok(self.scan_number(start, first_end, line, col)),
            c if c == '_' || c.is_ascii_alphabetic() => {
                Ok(self.scan_ident(start, first_end, line, col))
            }
            c => Err(LexError::UnexpectedChar {
                ch: c,
                span: Span {
                    start: offset(start),
                    end: offset(first_end),
                    line,
                    col,
                    file: self.file,
                },
            }),
        }
    }

    fn scan_ident(&mut self, start: usize, first_end: usize, line: u32, col: u32) -> Token<'src> {
        let mut end = first_end;
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                let (idx, c) = self.bump().expect("peeked char must be consumable");
                end = idx + c.len_utf8();
            } else {
                break;
            }
        }
        let lexeme = &self.source[start..end];
        let kind = if lexeme == "template" {
            TokenKind::Template
        } else {
            TokenKind::Ident
        };
        Token {
            kind,
            lexeme,
            span: Span {
                start: offset(start),
                end: offset(end),
                line,
                col,
                file: self.file,
            },
        }
    }

    fn scan_number(&mut self, start: usize, first_end: usize, line: u32, col: u32) -> Token<'src> {
        let mut end = first_end;
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                let (idx, c) = self.bump().expect("peeked char must be consumable");
                end = idx + c.len_utf8();
            } else {
                break;
            }
        }
        let lexeme = &self.source[start..end];
        Token {
            kind: TokenKind::Number,
            lexeme,
            span: Span {
                start: offset(start),
                end: offset(end),
                line,
                col,
                file: self.file,
            },
        }
    }

    /// `start` is the byte offset of the opening `"`. The opening quote
    /// is always exactly one byte (ASCII), so the content begins at
    /// `start + 1`. There is no escape handling of any kind: a `\` is an
    /// ordinary content character, and `{{`/`}}` interpolation markers
    /// (resolved later at codegen time) are never inspected here.
    fn scan_string(&mut self, start: usize, line: u32, col: u32) -> Result<Token<'src>, LexError> {
        let content_start = start + 1;
        let mut content_end = content_start;
        loop {
            match self.peek_char() {
                Some('"') => {
                    let (idx, c) = self.bump().expect("peeked '\"' must be consumable");
                    let end = idx + c.len_utf8();
                    return Ok(Token {
                        kind: TokenKind::Str,
                        lexeme: &self.source[content_start..content_end],
                        span: Span {
                            start: offset(start),
                            end: offset(end),
                            line,
                            col,
                            file: self.file,
                        },
                    });
                }
                Some('\n') | None => {
                    return Err(LexError::UnterminatedString {
                        span: Span {
                            start: offset(start),
                            end: offset(content_end),
                            line,
                            col,
                            file: self.file,
                        },
                    });
                }
                Some(_) => {
                    let (idx, c) = self.bump().expect("peeked char must be consumable");
                    content_end = idx + c.len_utf8();
                }
            }
        }
    }
}

impl<'src> Iterator for Lexer<'src> {
    type Item = Result<Token<'src>, LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_token() {
            Ok(tok) => {
                if tok.kind == TokenKind::Eof {
                    self.done = true;
                }
                Some(Ok(tok))
            }
            Err(err) => {
                self.done = true;
                Some(Err(err))
            }
        }
    }
}
