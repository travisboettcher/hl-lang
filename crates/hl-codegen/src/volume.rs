//! Classifies a `volume` entry's host side as a named volume or a bind
//! mount, confirmed against the real homelab: `syncthing-config` (a bare
//! identifier, no leading `/` or `.`) is a named volume; `/home/x/y` and
//! `./jellyfin` are both bind-mount paths — matching Docker's own
//! convention for telling the two apart.
//!
//! Since #60 this is only the *first* half of the question. A host side
//! classified as a named volume is a reference that must resolve to a
//! top-level `volume` declaration (`hl_codegen::resolve_volumes`, and
//! `CodegenError::UnknownVolume` when it doesn't); it no longer conjures
//! its own `volumes:` entry out of the string alone. A bind-mount path
//! is unchanged and needs no declaration, exactly as Docker requires
//! none.

/// `Some(name)` if `host` is a named-volume identifier; `None` if it's a
/// bind-mount path.
pub fn classify_named_volume(host: &str) -> Option<&str> {
    if host.starts_with('/') || host.starts_with('.') {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_identifier_is_named_volume() {
        assert_eq!(
            classify_named_volume("syncthing-config"),
            Some("syncthing-config")
        );
    }

    #[test]
    fn absolute_path_is_bind_mount() {
        assert_eq!(classify_named_volume("/home/x/y"), None);
    }

    #[test]
    fn relative_path_is_bind_mount() {
        assert_eq!(classify_named_volume("./jellyfin"), None);
    }

    #[test]
    fn parent_relative_path_is_bind_mount() {
        assert_eq!(classify_named_volume("../x"), None);
    }
}
