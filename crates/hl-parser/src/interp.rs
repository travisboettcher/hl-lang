//! The one scanner for `{{binding}}` interpolation inside string
//! content, per docs/DESIGN.md's lexical grammar section: `{{...}}` is
//! "not part of the lexical grammar — it's ordinary content inside a
//! `STRING` token, resolved later".
//!
//! "Later" is now two places rather than one. Composition
//! ([`mod@crate::compose`]) resolves a binding naming one of the enclosing
//! template's own parameters, since that is the only stage where the
//! invocation's bound arguments still exist; codegen resolves `{{name}}`
//! against the service it is generating, since that is the only stage
//! that knows which service a template's contribution ended up on. What
//! the two must never disagree about is where a binding *starts and
//! ends* — a second copy of this loop would be exactly the "two code
//! paths that could disagree about escaping, interpolation, or quoting"
//! this project rejects elsewhere — so both call [`resolve_with`] and
//! differ only in what they do with a binding once it's found.

/// Rewrites every `{{binding}}` occurrence in `text` by handing each
/// binding's name to `lookup`.
///
/// `lookup` returns `Ok(Some(value))` to substitute, `Ok(None)` to leave
/// the occurrence exactly as written (`{{binding}}`, braces included)
/// for a later stage to resolve, or `Err` to fail the whole resolution.
/// Deferral is what lets composition resolve a template's own parameters
/// while leaving `{{name}}` untouched for codegen.
///
/// An unclosed `{{` is not an error here: this is string *content*, not
/// a token, and lex-validating hl-lang source is not this function's
/// job. Everything from the `{{` on is passed through verbatim.
pub fn resolve_with<E>(
    text: &str,
    mut lookup: impl FnMut(&str) -> Result<Option<String>, E>,
) -> Result<String, E> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            out.push_str("{{");
            rest = after_open;
            break;
        };
        let binding = &after_open[..end];
        match lookup(binding)? {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str("{{");
                out.push_str(binding);
                out.push_str("}}");
            }
        }
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `lookup` that never fails, so the test can ignore the error
    /// type entirely.
    fn resolve(text: &str, lookup: impl FnMut(&str) -> Option<String>) -> String {
        let mut lookup = lookup;
        resolve_with(text, |b| Ok::<_, ()>(lookup(b))).expect("infallible lookup")
    }

    #[test]
    fn substitutes_a_resolved_binding() {
        assert_eq!(
            resolve("{{name}}.example.com", |_| Some("syncthing".to_string())),
            "syncthing.example.com"
        );
    }

    #[test]
    fn leaves_a_deferred_binding_exactly_as_written() {
        assert_eq!(resolve("a {{name}} b", |_| None), "a {{name}} b");
    }

    #[test]
    fn resolves_and_defers_within_one_string() {
        let out = resolve("{{host}}:{{name}}", |b| {
            (b == "host").then(|| "example.com".to_string())
        });
        assert_eq!(out, "example.com:{{name}}");
    }

    #[test]
    fn passes_through_text_with_no_interpolation() {
        assert_eq!(resolve("plain text", |_| None), "plain text");
    }

    #[test]
    fn resolves_multiple_occurrences() {
        assert_eq!(resolve("{{n}}-{{n}}", |_| Some("x".to_string())), "x-x");
    }

    #[test]
    fn an_unclosed_open_brace_passes_through() {
        assert_eq!(resolve("a {{name", |_| Some("x".to_string())), "a {{name");
    }

    #[test]
    fn a_lookup_error_stops_the_whole_resolution() {
        let err = resolve_with("{{a}}{{b}}", |b| match b {
            "a" => Ok(Some("ok".to_string())),
            _ => Err("boom"),
        });
        assert_eq!(err, Err("boom"));
    }
}
