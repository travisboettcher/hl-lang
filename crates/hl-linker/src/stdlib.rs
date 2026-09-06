//! The standard library of `.hll` modules the compiler carries inside
//! itself, and the `std:` namespace a `use` reaches them through.
//!
//! # Why the bytes are compiled in
//!
//! Every module here is [`include_str!`]d into the binary rather than
//! looked up on disk. A search path — `$HLL_PATH`, `~/.hllc/lib`, a
//! directory beside the executable — would make what a given `use`
//! resolves to depend on the machine the compiler runs on, and today
//! nothing about a build does: `use` resolves relative to the importing
//! file, so a source tree plus a compiler version fully determines the
//! generated document. Compiling the bytes in keeps that property and
//! version-locks a module to the compiler that reads it, which matters
//! most for the module this namespace exists for — a Traefik template
//! that has to reproduce the label output of the very compiler linking
//! it (#259).
//!
//! # What this deliberately never becomes
//!
//! Out of scope, permanently, not "not yet": a search path, an
//! environment variable, a `~/.hllc/lib` directory, anything fetched
//! over a network, and any versioning scheme inside a module path
//! (`std:traefik@2`). One namespace, one source of bytes, moving with
//! the compiler. A module path that could name two different bodies is
//! the seam a package manager grows out of, and this is a compiler for
//! a config language.
//!
//! Because the modules move with the compiler, a `0.x` release can
//! change what one of them generates — docs/DESIGN.md's Imports section
//! says so, since it's the cost of not versioning module paths.

use std::path::PathBuf;

use crate::path::resolve_relative;

/// The prefix that sends a `use` path here instead of to
/// [`crate::path::resolve_relative`]. Reserved: see
/// [`strip_prefix`].
const PREFIX: &str = "std:";

/// The extension a module's virtual file name carries, so a diagnostic
/// about a bundled module names something that looks like the `.hll`
/// file it is.
const EXTENSION: &str = ".hll";

/// A table of bundled modules, as `(name, source)` pairs.
///
/// A plain slice, rather than a map: the table holds a handful of
/// entries, it's `const`, and a linear scan over it is both faster than
/// hashing and readable in a diagnostic that lists what's available.
pub(crate) type Registry = &'static [(&'static str, &'static str)];

/// Every module this compiler bundles.
///
/// Empty on purpose. The mechanism ships ahead of its first module
/// because that module — the Traefik template of #259 — has to be
/// proven to reproduce the compiler's built-in label output byte for
/// byte before it can replace it, which is #269's job rather than this
/// one's. Adding a module is one line here:
///
/// ```ignore
/// pub(crate) const BUNDLED: Registry = &[("traefik", include_str!("stdlib/traefik.hll"))];
/// ```
pub(crate) const BUNDLED: Registry = &[];

/// The module name a `std:`-prefixed `use` path names, or `None` for
/// any other path (which stays a relative path, resolved as it always
/// was).
///
/// This is what reserves the prefix: `use "std:traefik"` names the
/// bundled module whether or not a file called `std:traefik.hll` sits
/// beside the importing one, so the two namespaces can't be made to
/// collide by naming a file adversarially.
///
/// The cost is that the bare spelling is taken: a file literally named
/// `std:traefik.hll` no longer answers to `use "std:traefik.hll"`.
/// Reaching it takes an explicit relative path, `use
/// "./std:traefik.hll"` — which this deliberately leaves alone, since
/// rejecting it would remove a capability to buy nothing — and even
/// then the two render alike, because [`display_path`] spells a bundled
/// module the same way. An accepted, documented trade
/// (docs/DESIGN.md's Imports section): a `:` in a file name is rare
/// enough to make that a curiosity, and the alternative is a namespace
/// any user file can shadow by name.
pub(crate) fn strip_prefix(raw: &str) -> Option<&str> {
    raw.strip_prefix(PREFIX).map(canonical_name)
}

/// A module's name with an optional `.hll` extension trimmed off, so
/// `use "std:traefik"` in a user file and a sibling module's own
/// `use "traefik.hll"` name one module rather than two.
///
/// The two spellings exist because one namespace is addressed both ways:
/// from outside as a name, from inside as a relative file path (see
/// [`resolve_sibling`]). Canonicalizing here is what keeps the module
/// graph's memoization from loading the same bytes twice under two
/// identities.
fn canonical_name(raw: &str) -> &str {
    raw.strip_suffix(EXTENSION).unwrap_or(raw)
}

/// The module a *relative* `use` inside the bundled module `from`
/// names — resolved within the standard library, never against the
/// user's tree.
///
/// A bundled module sits in a namespace of its own, so `from`'s own
/// name is the base a relative path resolves against, exactly as an
/// importing file's directory is for a user file. Reusing
/// [`resolve_relative`] is what keeps the escape rules identical:
/// `None` here means the same absolute-or-climbing-out path it means
/// there.
pub(crate) fn resolve_sibling(from: &str, raw: &str) -> Option<String> {
    let resolved = resolve_relative(&PathBuf::from(format!("{from}{EXTENSION}")), raw)?;
    Some(canonical_name(&resolved.to_string_lossy()).to_string())
}

/// The source of the bundled module `name`, or `None` if `registry`
/// holds no such module.
pub(crate) fn source(registry: Registry, name: &str) -> Option<&'static str> {
    registry
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, source)| *source)
}

/// Every module name in `registry`, for the diagnostic that has to say
/// what a failed lookup could have named instead.
pub(crate) fn available(registry: Registry) -> Vec<String> {
    registry
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}

/// The path a bundled module renders as in a diagnostic, and the one
/// the graph's [`hl_parser::SourceMap`] interns for it.
///
/// It's a display identity only — the module graph keys bundled modules
/// by name, in a table of their own — so this never has to be a path no
/// user file could hold. It's spelled the way the `use` that reached
/// the module was, so a location in a bundled module traces straight
/// back to the import that pulled it in.
pub(crate) fn display_path(name: &str) -> PathBuf {
    PathBuf::from(format!("{PREFIX}{name}{EXTENSION}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: Registry = &[
        ("traefik", "template labels {}\n"),
        ("net", "network n {}\n"),
    ];

    #[test]
    fn a_std_prefixed_path_names_a_bundled_module() {
        assert_eq!(strip_prefix("std:traefik"), Some("traefik"));
    }

    #[test]
    fn the_extension_is_optional_in_a_std_prefixed_path() {
        assert_eq!(strip_prefix("std:traefik.hll"), Some("traefik"));
    }

    #[test]
    fn a_relative_path_names_no_bundled_module() {
        assert_eq!(strip_prefix("traefik.hll"), None);
        assert_eq!(strip_prefix("./std:traefik.hll"), None);
    }

    #[test]
    fn a_sibling_resolves_within_the_standard_library() {
        assert_eq!(
            resolve_sibling("traefik", "labels.hll"),
            Some("labels".to_string())
        );
    }

    #[test]
    fn a_sibling_resolves_against_the_importing_modules_own_directory() {
        assert_eq!(
            resolve_sibling("net/base", "../shared.hll"),
            Some("shared".to_string())
        );
    }

    #[test]
    fn a_sibling_path_climbing_out_of_the_standard_library_is_rejected() {
        assert_eq!(resolve_sibling("traefik", "../../etc/hostname"), None);
        assert_eq!(resolve_sibling("traefik", "/etc/hostname"), None);
    }

    #[test]
    fn source_is_found_by_name() {
        assert_eq!(source(REGISTRY, "net"), Some("network n {}\n"));
        assert_eq!(source(REGISTRY, "nope"), None);
        assert_eq!(source(BUNDLED, "traefik"), None);
    }

    #[test]
    fn available_lists_every_bundled_name() {
        assert_eq!(available(REGISTRY), vec!["traefik", "net"]);
        assert!(available(BUNDLED).is_empty());
    }

    #[test]
    fn a_module_renders_as_the_path_its_import_spelled() {
        assert_eq!(display_path("traefik"), PathBuf::from("std:traefik.hll"));
    }
}
