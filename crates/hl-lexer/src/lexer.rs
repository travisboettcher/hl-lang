use std::iter::Peekable;
use std::str::CharIndices;

use crate::error::LexError;
use crate::span::Span;
use crate::token::{Token, TokenKind};

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
}

impl<'src> Lexer<'src> {
    /// Creates a new lexer over `source`.
    pub fn new(source: &'src str) -> Self {
        Lexer {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            col: 1,
            done: false,
        }
    }

    /// Scans `source` in full, returning every token up to and including
    /// `Eof`, or the first [`LexError`] encountered.
    pub fn tokenize(source: &'src str) -> Result<Vec<Token<'src>>, LexError> {
        Lexer::new(source).collect()
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    /// Consumes and returns the next `(byte_offset, char)`, updating the
    /// running line/col position. A `\n` bumps `line` and resets `col` to
    /// 1; any other character (including `\r`, which never appears alone
    /// as a token) just bumps `col` — so a `\r\n` pair increments `line`
    /// exactly once, matching LF-only input.
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
                        start: pos,
                        end: pos,
                        line,
                        col,
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
                        start,
                        end: first_end,
                        line,
                        col,
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
            '-' => {
                if self.peek_char() == Some('>') {
                    let (idx, c) = self.bump().expect("peeked '>' must be consumable");
                    let end = idx + c.len_utf8();
                    Ok(Token {
                        kind: TokenKind::Arrow,
                        lexeme: &self.source[start..end],
                        span: Span {
                            start,
                            end,
                            line,
                            col,
                        },
                    })
                } else {
                    Err(LexError::DanglingDash {
                        span: Span {
                            start,
                            end: first_end,
                            line,
                            col,
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
                    start,
                    end: first_end,
                    line,
                    col,
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
                start,
                end,
                line,
                col,
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
                start,
                end,
                line,
                col,
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
                            start,
                            end,
                            line,
                            col,
                        },
                    });
                }
                Some('\n') | None => {
                    return Err(LexError::UnterminatedString {
                        span: Span {
                            start,
                            end: content_end,
                            line,
                            col,
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
