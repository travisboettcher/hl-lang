//! Parser for hl-lang: turns a token stream from `hl-lexer` into an AST,
//! and (via [`compose`]) resolves `template`/`with` composition into
//! fully-merged services.
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
//! [`parse`] is purely syntactic — it enforces known fields, correct
//! kinds, and no illegal duplicates, but doesn't resolve `template`/
//! `with` composition. [`compose::compose`] is a separate pass, run on
//! `parse`'s output, that resolves every `with`-list (per docs/DESIGN.md's
//! Composition section) into fully-merged services with no templates or
//! unresolved parameters left.
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
pub mod compose;
mod error;
mod parser;
pub mod schema;

pub use ast::{
    EnvEntry, EnvMap, Expose, Ident, Image, Literal, Network, Program, RawEntry, RawMap, RawValue,
    Reference, Restart, Service, ServiceFields, TemplateDecl, TemplateInvocation, TopDecl, UseDecl,
    VolumeEntry, VolumeMap,
};
pub use compose::{
    ComposeError, ComposedProgram, MapKeyCollision, SymbolResolver, compose, compose_with_resolver,
};
pub use error::{Expected, ParseError};
pub use hl_lexer::Span;
pub use parser::parse;
