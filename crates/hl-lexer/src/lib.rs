//! Lexer for hl-lang, the DSL that transpiles to Docker Compose YAML +
//! Traefik labels (see `docs/DESIGN.md` in the repo root for the full
//! grammar and motivation).
//!
//! This crate turns hl-lang source text into a stream of [`Token`]s. It
//! recognizes the language's lexical grammar exactly and no more:
//! identifiers, integer number literals, double-quoted string literals
//! (with the backslash escapes [`unescape`] decodes), the single
//! reserved word `template`, the punctuation set
//! `{ } [ ] ( ) : = -> , . $`, and `#`-to-end-of-line
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
//!
//! # Stability
//!
//! This crate is an implementation detail of the `hllc` compiler, not a
//! library offered for outside use. It is never published to crates.io —
//! a release is a git tag plus a GitHub Release carrying the `hllc`
//! binary (`release-plz.toml`'s `[workspace] publish = false` +
//! `git_only = true`) — and its version number only tracks `hllc`'s so
//! the whole workspace moves in lockstep.
//!
//! **No semver guarantee applies to this Rust API.** Types, public
//! fields, error enum variants, module paths, and function signatures
//! may change in any release, including a patch. What the version number
//! does promise is `.hll` source compatibility, the `hllc` CLI contract,
//! and generated-Compose semantics; see "What a version number promises"
//! in the repo README.

mod error;
mod escape;
mod lexer;
mod span;
mod token;

pub use error::LexError;
pub use escape::unescape;
pub use lexer::Lexer;
pub use span::{FileId, Location, SourceMap, Span};
pub use token::{Token, TokenKind};
