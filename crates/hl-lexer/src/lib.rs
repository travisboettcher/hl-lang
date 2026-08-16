//! Lexer for hl-lang, the DSL that transpiles to Docker Compose YAML +
//! Traefik labels (see `docs/DESIGN.md` in the repo root for the full
//! grammar and motivation).
//!
//! This crate turns hl-lang source text into a stream of [`Token`]s. It
//! recognizes the language's lexical grammar exactly and no more:
//! identifiers, integer number literals, double-quoted string literals
//! (no escape sequences), the single reserved word `template`, the
//! punctuation set `{ } [ ] ( ) : = -> , . $`, and `#`-to-end-of-line
//! comments. Every other keyword-shaped word (`service`, `with`, `as`,
//! `external`, `raw`, `defaults`, ...) is lexed as a plain [`TokenKind::Ident`]
//! — the lexer has no notion of which identifiers are meaningful; that is
//! resolved later by the parser against a schema table.
//!
//! # Example
//!
//! ```
//! use hl_lexer::{Lexer, TokenKind};
//!
//! let source = r#"
//! service jellyfin {
//!   image "jellyfin/jellyfin:latest"
//!   restart unless-stopped
//! }
//! "#;
//!
//! let tokens = Lexer::tokenize(source).expect("valid hl-lang source");
//! assert_eq!(tokens.first().unwrap().kind, TokenKind::Ident); // "service"
//! assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
//! ```

mod error;
mod lexer;
mod span;
mod token;

pub use error::LexError;
pub use lexer::Lexer;
pub use span::Span;
pub use token::{Token, TokenKind};
