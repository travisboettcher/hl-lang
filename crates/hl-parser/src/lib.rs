//! Parser for hl-lang: turns a token stream from `hl-lexer` into an AST.
//!
//! The parser is one generic, schema-table-driven engine — not one
//! function per keyword. A [`schema::TypeSchema`] table (in `schema.rs`)
//! describes each built-in type's shape (struct vs. map, primary field,
//! separator, uniqueness side, etc.); the parser consults it to decide
//! how to interpret whatever it's looking at, so adding a new leaf
//! concept to the language is a new schema-table row, not new parser
//! code. See `docs/DESIGN.md` in the repo root for the full grammar this
//! implements.
//!
//! **Scope of this milestone:** only the built-in types are supported —
//! `network`, `service`, `image`, `expose`, `volume`, `env`, `restart`,
//! `raw`, plus the reference-list fields `middleware`/`depends_on`/
//! `networks`. `template` declarations and the `with` field are rejected
//! with [`ParseError::TemplatesNotSupported`] rather than parsed —
//! template/`with` composition is a fast-follow milestone.
//!
//! # Example
//!
//! ```
//! let source = r#"
//! service jellyfin {
//!   image "jellyfin/jellyfin:latest"
//!   expose 8096 as "media.techdebtor.io"
//!   restart unless-stopped
//! }
//! "#;
//!
//! let program = hl_parser::parse(source).expect("valid hl-lang source");
//! assert_eq!(program.decls.len(), 1);
//! ```

mod ast;
mod error;
mod parser;
pub mod schema;

pub use ast::{
    EnvEntry, EnvMap, Expose, Ident, Image, Literal, Network, Program, RawEntry, RawMap, RawValue,
    Reference, Restart, Service, TopDecl, VolumeEntry, VolumeMap,
};
pub use error::{Expected, ParseError};
pub use hl_lexer::Span;
pub use parser::parse;
