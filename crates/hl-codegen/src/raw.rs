//! Transcribes `raw`'s schema-free [`RawValue`] tree into
//! [`serde_yaml::Value`] — mechanical passthrough, no domain modeling,
//! matching `raw`'s own design intent (arbitrary Compose service keys
//! the compiler has no opinion about, e.g. `privileged`/`devices`/
//! `security_opt` in the real `cadvisor` service).

use std::collections::HashMap;

use hl_parser::{Literal, RawValue};

use crate::{CodegenError, interp};

/// Transcribes one `RawValue` into YAML, resolving `{{}}` interpolation
/// in every string it contains along the way.
pub fn to_yaml(
    value: &RawValue,
    bindings: &HashMap<&str, &str>,
) -> Result<serde_yaml::Value, CodegenError> {
    match value {
        RawValue::Literal(lit) => literal_to_yaml(lit, bindings),
        RawValue::List(items, _) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(to_yaml(item, bindings)?);
            }
            Ok(serde_yaml::Value::Sequence(out))
        }
        RawValue::Map(entries, _) => {
            let mut map = serde_yaml::Mapping::new();
            for (key, val) in entries {
                let resolved_key = interp::resolve(key.text(), bindings, key.span())?;
                map.insert(
                    serde_yaml::Value::String(resolved_key),
                    to_yaml(val, bindings)?,
                );
            }
            Ok(serde_yaml::Value::Mapping(map))
        }
    }
}

/// A `raw` literal's YAML type: numbers stay numbers; the bare
/// identifiers `true`/`false` become actual YAML booleans (matching the
/// real `cadvisor` example's `privileged: true`, an actual Compose
/// boolean, not the string `"true"`); every other literal — including a
/// quoted `"true"`, since the user explicitly asked for a string there —
/// becomes a YAML string.
fn literal_to_yaml(
    lit: &Literal,
    bindings: &HashMap<&str, &str>,
) -> Result<serde_yaml::Value, CodegenError> {
    match lit {
        Literal::Number { value, .. } => Ok(serde_yaml::Value::Number((*value).into())),
        Literal::Ident(text, span) if text == "true" || text == "false" => {
            let _ = span; // no interpolation needed for a bool literal
            Ok(serde_yaml::Value::Bool(text == "true"))
        }
        Literal::Str(_, span) | Literal::Ident(_, span) => {
            let resolved = interp::resolve(lit.text(), bindings, *span)?;
            Ok(serde_yaml::Value::String(resolved))
        }
        // Composition is supposed to have substituted this away already,
        // so reaching here means a hole in that invariant rather than
        // anything the user wrote wrong — but a compiler bug should
        // still surface as a located diagnostic, not a panic in the
        // middle of a build (#62).
        Literal::Param(name, span) => Err(CodegenError::UnsubstitutedParameter {
            param: name.clone(),
            span: *span,
        }),
    }
}

/// A literal used directly as a scalar Compose value (e.g. `expose`'s
/// port list) — numbers stay numbers, everything else is a plain string
/// with no bool coercion (unlike [`literal_to_yaml`], which is scoped to
/// `raw`'s more permissive passthrough semantics).
pub fn scalar_value(lit: &Literal) -> serde_yaml::Value {
    match lit {
        Literal::Number { value, .. } => serde_yaml::Value::Number((*value).into()),
        _ => serde_yaml::Value::String(lit.text().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_parser::Span;

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        }
    }

    fn bindings() -> HashMap<&'static str, &'static str> {
        HashMap::new()
    }

    #[test]
    fn literal_transcribes_to_scalar() {
        let lit = Literal::Str("unconfined".to_string(), span());
        let yaml = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap();
        assert_eq!(yaml, serde_yaml::Value::String("unconfined".to_string()));
    }

    #[test]
    fn true_ident_becomes_bool() {
        let lit = Literal::Ident("true".to_string(), span());
        let yaml = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap();
        assert_eq!(yaml, serde_yaml::Value::Bool(true));
    }

    #[test]
    fn false_ident_becomes_bool() {
        let lit = Literal::Ident("false".to_string(), span());
        let yaml = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap();
        assert_eq!(yaml, serde_yaml::Value::Bool(false));
    }

    #[test]
    fn other_ident_stays_string_not_bool() {
        let lit = Literal::Ident("unconfined".to_string(), span());
        let yaml = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap();
        assert_eq!(yaml, serde_yaml::Value::String("unconfined".to_string()));
    }

    #[test]
    fn quoted_true_stays_string() {
        let lit = Literal::Str("true".to_string(), span());
        let yaml = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap();
        assert_eq!(yaml, serde_yaml::Value::String("true".to_string()));
    }

    /// A `Literal::Param` reaching codegen is a compiler-invariant
    /// violation, but it degrades to an error rather than the panic it
    /// used to be (#62).
    #[test]
    fn surviving_param_is_an_error_not_a_panic() {
        let lit = Literal::Param("x".to_string(), span());
        let err = to_yaml(&RawValue::Literal(lit), &bindings()).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::UnsubstitutedParameter { param, .. } if param == "x"
        ));
    }

    #[test]
    fn list_transcribes_recursively() {
        let items = vec![RawValue::Literal(Literal::Str(
            "/dev/kmsg".to_string(),
            span(),
        ))];
        let yaml = to_yaml(&RawValue::List(items, span()), &bindings()).unwrap();
        assert_eq!(
            yaml,
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("/dev/kmsg".to_string())])
        );
    }

    #[test]
    fn nested_map_transcribes_recursively() {
        let entries = vec![(
            Literal::Ident("seccomp".to_string(), span()),
            RawValue::Literal(Literal::Str("unconfined".to_string(), span())),
        )];
        let yaml = to_yaml(&RawValue::Map(entries, span()), &bindings()).unwrap();
        let mut expected = serde_yaml::Mapping::new();
        expected.insert(
            serde_yaml::Value::String("seccomp".to_string()),
            serde_yaml::Value::String("unconfined".to_string()),
        );
        assert_eq!(yaml, serde_yaml::Value::Mapping(expected));
    }
}
