//! Resolves `{{ident}}` interpolation inside string content, per
//! docs/DESIGN.md's lexical grammar section: `{{name}}` is "an implicit
//! binding to the enclosing service's own name... not part of the
//! lexical grammar — it's ordinary content inside a STRING token,
//! resolved later at codegen time." This is that later resolution.

use std::collections::HashMap;

use hl_parser::Span;

use crate::CodegenError;

/// Resolves every `{{ident}}` occurrence in `text` against `bindings`.
/// An unrecognized binding is a hard error ([`CodegenError::UnknownInterpolation`]),
/// not a silent passthrough of literal `{{typo}}` text into generated
/// YAML. `span` is used to locate the error — it should be the span of
/// the literal `text` came from.
pub fn resolve(
    text: &str,
    bindings: &HashMap<&str, &str>,
    span: Span,
) -> Result<String, CodegenError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            // No closing `}}` — not our job to lex-validate hl-lang
            // source here, just pass the rest through unchanged.
            out.push_str("{{");
            rest = after_open;
            break;
        };
        let binding = &after_open[..end];
        match bindings.get(binding) {
            Some(value) => out.push_str(value),
            None => {
                return Err(CodegenError::UnknownInterpolation {
                    binding: binding.to_string(),
                    span,
                });
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
    use hl_parser::FileId;

    fn bindings() -> HashMap<&'static str, &'static str> {
        HashMap::from([("name", "syncthing")])
    }

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
            file: FileId::ANONYMOUS,
        }
    }

    #[test]
    fn resolves_known_binding() {
        let out = resolve("{{name}}.internal.techdebtor.io", &bindings(), span()).unwrap();
        assert_eq!(out, "syncthing.internal.techdebtor.io");
    }

    #[test]
    fn passes_through_text_with_no_interpolation() {
        let out = resolve("plain text", &bindings(), span()).unwrap();
        assert_eq!(out, "plain text");
    }

    #[test]
    fn resolves_multiple_occurrences() {
        let out = resolve("{{name}}-{{name}}", &bindings(), span()).unwrap();
        assert_eq!(out, "syncthing-syncthing");
    }

    #[test]
    fn unknown_binding_is_error() {
        let err = resolve("{{typo}}", &bindings(), span()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnknownInterpolation { binding, .. } if binding == "typo"
        ));
    }
}
