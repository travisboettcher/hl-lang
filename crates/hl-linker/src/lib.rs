//! Resolves a real, on-disk `use` graph into one [`ComposedProgram`] —
//! the same type [`fn@hl_parser::compose`] produces for a single file, so
//! `hl-codegen` needs no changes to consume either. [`mod@hl_parser::compose`]'s
//! own doc explains what composition means; this crate is only
//! responsible for the part `compose()` deliberately doesn't do: turning
//! `use PATH as ALIAS` declarations into an actual loaded, alias-resolved
//! [`hl_parser::SymbolResolver`] over more than one file.
//!
//! # Example
//!
//! ```
//! use hl_linker::{InMemoryLoader, link};
//!
//! let mut loader = InMemoryLoader::default();
//! loader.add("docker.hll", "network traefik-net {\n  external\n}\n");
//! loader.add(
//!     "service.hll",
//!     "use \"docker.hll\" as traefik\n\
//!      service jellyfin {\n  image \"jellyfin/jellyfin\"\n  networks [traefik.traefik-net]\n}\n",
//! );
//!
//! let linked = link(std::path::Path::new("service.hll"), &loader).unwrap();
//! assert_eq!(linked.program.services.len(), 1);
//! assert!(linked.warnings.is_empty());
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
mod graph;
mod loader;
mod path;
mod warning;

pub use error::LinkError;
pub use loader::{FileLoader, FsLoader, InMemoryLoader};
pub use warning::LinkWarning;

use std::path::Path;

use hl_parser::{ComposedProgram, compose_with_resolver};

/// What [`link`] produces: the composed program, plus every non-fatal
/// diagnostic raised on the way to it.
///
/// The warnings ride *alongside* the program rather than inside it
/// because [`ComposedProgram`] belongs to `hl-parser`, which knows
/// nothing about `use` graphs — this is the linker's own channel, and it
/// is the whole of the non-fatal machinery this stage needs (#80). The
/// caller decides what to do with the list; `hllc` prints each entry to
/// stderr and carries on, exit code untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct Linked {
    pub program: ComposedProgram,
    /// In load order — the entry file first, then each imported file as
    /// it was reached. Empty for the overwhelmingly common case of a
    /// graph with nothing dropped.
    pub warnings: Vec<LinkWarning>,
}

/// Loads `entry` and its whole transitive `use` graph via `loader` (see
/// [`FileLoader`]) — eagerly, in one pass, before composing anything —
/// then resolves every `template`/`with` composition and cross-file
/// `alias.name` reference into one [`ComposedProgram`], returned inside a
/// [`Linked`] together with any [`LinkWarning`]s.
///
/// The returned [`ComposedProgram`] carries the graph's
/// [`hl_parser::SourceMap`] in its `files` field, so a codegen
/// diagnostic raised further down the pipeline can still name the file
/// any span came from — including a field that a service inherited from
/// a template in some imported file (#75). Errors and warnings raised
/// here resolve their own spans the same way before rendering.
pub fn link(entry: &Path, loader: &dyn FileLoader) -> Result<Linked, LinkError> {
    let mut graph = graph::build(entry, loader)?;
    let (networks, volumes, services) = graph.take_entry();
    let warnings = graph.take_warnings();
    let entry_scope = graph.entry_scope();
    let mut composed = compose_with_resolver(networks, volumes, services, entry_scope, &graph)
        .map_err(|err| LinkError::compose(err, graph.files()))?;
    composed.files = graph.files().clone();
    Ok(Linked {
        program: composed,
        warnings,
    })
}
