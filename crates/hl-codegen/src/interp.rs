//! Resolves `{{ident}}` interpolation inside string content, per
//! docs/DESIGN.md's lexical grammar section: `{{name}}` is "an implicit
//! binding to the enclosing service's own name... not part of the
//! lexical grammar — it's ordinary content inside a STRING token,
//! resolved later at codegen time."
//!
//! This is the *last* of those resolutions. Composition resolves a
//! binding naming one of the enclosing template's own parameters first
//! (#266) and leaves the rest alone, so what reaches here is what only a
//! service can answer. Both stages scan for a binding with
//! [`hl_parser::interp::resolve_with`] rather than a loop each — see its
//! own doc for why there is exactly one scanner.

use std::collections::HashMap;

use hl_parser::{Span, interp};

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
    // Never defers (`Ok(None)`): this is the end of the pipeline, so a
    // binding nothing here answers is a binding nothing ever will.
    interp::resolve_with(text, |binding| match bindings.get(binding) {
        Some(value) => Ok(Some((*value).to_string())),
        None => Err(CodegenError::UnknownInterpolation {
            binding: binding.to_string(),
            span,
        }),
    })
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
