//! Builds one service's `labels:` list from the `labels` field it wrote.
//!
//! Every label this module emits is a label the author wrote. That is
//! the whole of it since #271: this file used to compute Traefik's
//! router and service labels from `router`, `expose` and
//! `traefik { disable }` blocks, and those are gone — routing is
//! `std:traefik`'s, written in `.hll` like any other template. What is
//! left is the part that was never Traefik-shaped: resolve each entry's
//! interpolation, refuse a key that would name a different label than
//! the one written, and refuse two entries that land on one key.

use std::collections::HashMap;

use hl_parser::{ServiceFields, Span};

use crate::{CodegenError, interp};

/// Rejects a label key holding a character that would make it stand for
/// a *different* key than the one written.
///
/// `=` is the whole of the danger. Docker splits a label string at its
/// first `=`, so an entry writing the key `a=b` with the value `c`
/// produces the key `a` holding `b=c` — a forged label rather than a
/// corrupted one, and one [`compute`]'s own duplicate check cannot see,
/// since the key it compares is the whole `a=b`. So this is
/// load-bearing for that check rather than general hygiene: without it,
/// a key spelled `traefik.docker.network=x` slips a second
/// `traefik.docker.network` label past a check written to make exactly
/// that impossible.
///
/// Control characters ride along because a string literal has escape
/// sequences since #181, so a newline in a key is writable, and no label
/// key holds one for any legitimate reason.
fn reject_unsafe_label_key(key: &str, span: Span) -> Result<(), CodegenError> {
    match key.chars().find(|c| *c == '=' || c.is_control()) {
        Some(character) => Err(CodegenError::UnsafeLabelKey {
            key: key.to_string(),
            character,
            span,
        }),
        None => Ok(()),
    }
}

/// This service's `labels:` list, in composed source order (#243).
///
/// Each entry's key and value are interpolated first, and that matters
/// for both checks below: the text that reaches the label is the
/// interpolated text, so that is the text the safety check and the
/// duplicate check both have to see.
///
/// Two entries landing on one key is [`CodegenError::DuplicateLabelKey`]
/// rather than a last-one-wins. It is only reachable *here* — two keys
/// spelled alike in source are already a parse error — because two
/// different spellings can resolve to one key once `{{name}}` is
/// substituted, which the parser can't see. Refusing is the same answer
/// the parser gives the spellings it can see, and the same answer #243
/// gave a hand-written key colliding with a computed one, back when this
/// module computed any.
pub fn compute(
    fields: &ServiceFields,
    bindings: &HashMap<&str, &str>,
) -> Result<Vec<String>, CodegenError> {
    let mut labels: Vec<String> = Vec::with_capacity(fields.labels.entries.len());
    let mut seen: Vec<(String, Span)> = Vec::with_capacity(fields.labels.entries.len());
    for entry in &fields.labels.entries {
        let key = interp::resolve(entry.key.text(), bindings, entry.key.span())?;
        // A list value renders comma-joined (#288), which is the same
        // separator a list argument interpolates with — one convention,
        // so a reader who has met either has met both. Each item
        // interpolates on its own, so `{{name}}` works inside a list
        // exactly as it does in a single value.
        let value = entry
            .value
            .join(|lit| interp::resolve(lit.text(), bindings, lit.span()))?;
        reject_unsafe_label_key(&key, entry.key.span())?;
        if let Some((_, first)) = seen.iter().find(|(held, _)| *held == key) {
            return Err(CodegenError::DuplicateLabelKey {
                key,
                first: *first,
                span: entry.span,
            });
        }
        seen.push((key.clone(), entry.span));
        labels.push(format!("{key}={value}"));
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_parser::{FileId, LabelEntry, LabelMap, LabelValue, Literal};

    fn span() -> Span {
        Span {
            file: FileId::ANONYMOUS,
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        }
    }

    fn bindings() -> HashMap<&'static str, &'static str> {
        HashMap::from([("name", "web")])
    }

    fn fields_with(entries: Vec<(&str, &str)>) -> ServiceFields {
        ServiceFields {
            labels: LabelMap {
                entries: entries
                    .into_iter()
                    .map(|(k, v)| LabelEntry {
                        key: Literal::Str(k.to_string(), span()),
                        value: LabelValue::Scalar(Literal::Str(v.to_string(), span())),
                        span: span(),
                    })
                    .collect(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn entries_emit_in_source_order() {
        let fields = fields_with(vec![("b.key", "2"), ("a.key", "1")]);
        assert_eq!(
            compute(&fields, &bindings()).unwrap(),
            vec!["b.key=2".to_string(), "a.key=1".to_string()],
            "source order, not sorted — a reader's diff should match what they wrote"
        );
    }

    /// `{{name}}` resolves on both halves, which is what makes the
    /// duplicate check below reachable at all.
    #[test]
    fn both_halves_interpolate() {
        let fields = fields_with(vec![("com.example.{{name}}.owner", "{{name}}-team")]);
        assert_eq!(
            compute(&fields, &bindings()).unwrap(),
            vec!["com.example.web.owner=web-team".to_string()]
        );
    }

    /// Two spellings, one key after interpolation — the collision the
    /// parser's own duplicate-key check can't see.
    #[test]
    fn two_spellings_resolving_to_one_key_collide() {
        let fields = fields_with(vec![
            ("com.example.web", "1"),
            ("com.example.{{name}}", "2"),
        ]);
        match compute(&fields, &bindings()) {
            Err(CodegenError::DuplicateLabelKey { key, .. }) => {
                assert_eq!(key, "com.example.web");
            }
            other => panic!("expected DuplicateLabelKey, got {other:?}"),
        }
    }

    /// The check that keeps the one above honest: an `=` in a key would
    /// otherwise let a second label through under a key this compares as
    /// something else.
    #[test]
    fn an_equals_in_a_key_is_refused() {
        let fields = fields_with(vec![("com.example=forged", "1")]);
        match compute(&fields, &bindings()) {
            Err(CodegenError::UnsafeLabelKey { character, .. }) => assert_eq!(character, '='),
            other => panic!("expected UnsafeLabelKey, got {other:?}"),
        }
    }

    /// A newline became writable with #181's escape sequences, and no
    /// label key holds one on purpose.
    #[test]
    fn a_control_character_in_a_key_is_refused() {
        let fields = fields_with(vec![("com.example\nowner", "1")]);
        match compute(&fields, &bindings()) {
            Err(CodegenError::UnsafeLabelKey { character, .. }) => assert_eq!(character, '\n'),
            other => panic!("expected UnsafeLabelKey, got {other:?}"),
        }
    }

    /// A service that writes no `labels` emits none — no computed list
    /// is left for it to inherit.
    #[test]
    fn no_labels_field_emits_nothing() {
        assert!(
            compute(&fields_with(vec![]), &bindings())
                .unwrap()
                .is_empty()
        );
    }
}
