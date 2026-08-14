//! Path identity for the module graph — deliberately **not**
//! [`std::fs::canonicalize`] (which requires the path to exist on disk
//! and follows symlinks): a purely lexical join-then-collapse gives
//! [`crate::loader::FsLoader`] and [`crate::loader::InMemoryLoader`]
//! identical path-identity/caching behavior with no special-casing, and
//! never touches disk itself (graph-building's own [`crate::loader::FileLoader::read`]
//! calls are the only I/O).

use std::path::{Component, Path, PathBuf};

/// Resolves a `use` decl's `raw` path (as written in source) relative to
/// `from`'s own parent directory — per docs/DESIGN.md's import-scoping
/// rule, always relative to the *importing file's own location*, never
/// the entry file's location or the current working directory — then
/// lexically collapses `.`/`..` components so two spellings of the same
/// file (e.g. `a/b.hll` and `a/../a/b.hll`) resolve to the same
/// [`PathBuf`] identity for the module graph's memoization.
pub(crate) fn resolve_relative(from: &Path, raw: &str) -> PathBuf {
    let base = from.parent().unwrap_or_else(|| Path::new(""));
    normalize(&base.join(raw))
}

/// Lexically collapses `.`/`..` components in `path`, without touching
/// disk. A leading `..` (one that would climb above whatever root the
/// path is relative to) is preserved as-is, matching how `.hll` paths
/// only ever compare against each other and never actually need
/// resolving against a filesystem root.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_to_importing_files_own_directory() {
        let from = Path::new("services/foo/service.hll");
        assert_eq!(
            resolve_relative(from, "../../shared/templates.hll"),
            PathBuf::from("shared/templates.hll")
        );
    }

    #[test]
    fn collapses_dot_and_dot_dot_components() {
        assert_eq!(
            normalize(Path::new("a/./b/../c.hll")),
            PathBuf::from("a/c.hll")
        );
    }

    #[test]
    fn sibling_file_resolves_relative_to_same_directory() {
        let from = Path::new("templates/web.hll");
        assert_eq!(
            resolve_relative(from, "docker.hll"),
            PathBuf::from("templates/docker.hll")
        );
    }

    #[test]
    fn entry_file_with_no_parent_resolves_relative_to_empty_base() {
        let from = Path::new("service.hll");
        assert_eq!(
            resolve_relative(from, "templates.hll"),
            PathBuf::from("templates.hll")
        );
    }
}
