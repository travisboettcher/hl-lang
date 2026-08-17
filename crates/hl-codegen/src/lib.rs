//! Codegen for hl-lang: walks a [`hl_parser::ComposedProgram`] (the
//! output of `hl_parser::compose`, with every `template`/`with`
//! composition already resolved) and emits one Docker Compose YAML
//! document covering every service in the program, with Traefik labels
//! on each service's own `labels:` list — required, not stylistic,
//! since Traefik's Docker provider reads labels off container metadata
//! directly.
//!
//! One input program produces one output document, which may hold
//! multiple services (mirroring how a real multi-service Compose
//! project, e.g. an Authentik-style app+database pair, is one file
//! today) — not one file per service.
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
//! and generated-Compose *semantics* — what the emitted document tells
//! Compose to do. The exact bytes are not promised: key ordering,
//! quoting, and formatting of the YAML may change without a major bump,
//! so don't diff generated output across `hllc` versions expecting
//! stability. See "What a version number promises" in the repo README.

mod doc;
mod interp;
mod labels;
mod raw;
mod volume;

use std::collections::HashMap;
use std::fmt;

use hl_parser::Span;
use hl_parser::{ComposedProgram, Network, Reference, Service};
use indexmap::IndexMap;

/// The result of running codegen on a [`ComposedProgram`]: one combined
/// Compose document.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedProgram {
    pub yaml: String,
}

/// `(network identifier, its Compose doc)` pairs to merge into the
/// program's top-level `networks:` section.
type NetworkDocs = Vec<(String, doc::NetworkDoc)>;

/// An error raised while generating Compose YAML from a composed
/// program. Mirrors [`hl_parser::ParseError`]/[`hl_parser::ComposeError`]'s
/// existing span-carrying, no-recovery style.
#[derive(Debug, Clone, PartialEq)]
pub enum CodegenError {
    /// A service's `networks [x]` references a network with no matching
    /// top-level `network` declaration in the same program. A hard
    /// error, not silent implicit-network creation — every real network
    /// reference in the target homelab has a corresponding declaration.
    UnknownNetwork {
        service: String,
        network: String,
        span: Span,
    },
    /// A service declares more than one `external` network, so which
    /// one's real name belongs in `traefik.docker.network=` is
    /// undecided.
    AmbiguousExternalNetwork {
        service: String,
        candidates: Vec<String>,
        span: Span,
    },
    /// A `{{binding}}` interpolation used a name codegen doesn't
    /// recognize (today, the only valid one is `name`).
    UnknownInterpolation { binding: String, span: Span },
    /// A service has no `image` set — required for a valid Compose
    /// service block. The parser doesn't enforce this (see
    /// [`hl_parser::Service`]'s doc); codegen must.
    MissingImage { service: String, span: Span },
    /// A `$param` reference survived composition and reached codegen.
    /// Composition is supposed to substitute every one of them (see
    /// [`hl_parser::Literal::Param`]'s doc), so this is a compiler
    /// invariant rather than an ordinary user error — but it's reported
    /// as a diagnostic anyway, not a panic: a gap in that invariant
    /// (`defaults` bypassing argument binding was one, #62) should
    /// degrade to a located error message the user can act on, never
    /// take the process down.
    UnsubstitutedParameter { param: String, span: Span },
    /// A value destined for a Traefik label contains a character that
    /// would change the meaning of the label it's spliced into — the
    /// canonical case being a backtick in `expose.host`, which closes
    /// the ``Host(`...`)`` rule early and lets everything after it be
    /// read as more rule grammar. Rejected rather than escaped:
    /// Traefik's rule grammar has no escape for that backtick, so there
    /// is nothing to escape *to*.
    UnsafeLabelValue {
        field: &'static str,
        character: char,
        span: Span,
    },
}

impl CodegenError {
    pub fn span(&self) -> Span {
        match self {
            CodegenError::UnknownNetwork { span, .. }
            | CodegenError::AmbiguousExternalNetwork { span, .. }
            | CodegenError::UnknownInterpolation { span, .. }
            | CodegenError::MissingImage { span, .. }
            | CodegenError::UnsubstitutedParameter { span, .. }
            | CodegenError::UnsafeLabelValue { span, .. } => *span,
        }
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span = self.span();
        match self {
            CodegenError::UnknownNetwork {
                service, network, ..
            } => write!(
                f,
                "{}:{}: service `{service}` references undeclared network `{network}`",
                span.line, span.col
            ),
            CodegenError::AmbiguousExternalNetwork {
                service,
                candidates,
                ..
            } => write!(
                f,
                "{}:{}: service `{service}` declares more than one external network ({}) — which real network name should `traefik.docker.network` use?",
                span.line,
                span.col,
                candidates.join(", ")
            ),
            CodegenError::UnknownInterpolation { binding, .. } => write!(
                f,
                "{}:{}: unknown interpolation `{{{{{binding}}}}}`",
                span.line, span.col
            ),
            CodegenError::MissingImage { service, .. } => write!(
                f,
                "{}:{}: service `{service}` has no `image` set",
                span.line, span.col
            ),
            CodegenError::UnsubstitutedParameter { param, .. } => write!(
                f,
                "{}:{}: template parameter `${param}` was never bound to an argument",
                span.line, span.col
            ),
            // The offending character is rendered with `Debug` rather
            // than wrapped in backticks like every other quoted name
            // here: a backtick is the single most likely value, and
            // backtick-quoting a backtick reads as an unbroken run of
            // three.
            CodegenError::UnsafeLabelValue {
                field, character, ..
            } => {
                // A comma in `expose.entrypoint` gets an extra sentence.
                // It's the one rejection here that used to be *accepted*
                // — `entrypoint "web,websecure"` was how you attached a
                // router to several entry points before `entrypoint`
                // became a list — so it's the one a user is likely to
                // hit by writing something that was correct yesterday,
                // or by pasting a value straight out of Traefik's own
                // docs. Pointing at the list form turns a dead end into
                // a one-line fix. Note this is a diagnostic affordance,
                // not a semantic carve-out: the value is still rejected,
                // exactly like every other metacharacter.
                let hint = if *field == "expose.entrypoint" && *character == ',' {
                    " — `entrypoint` is a list, so write the entry points as separate items (`entrypoint web, websecure`) and let `hllc` join them"
                } else {
                    ""
                };
                write!(
                    f,
                    "{}:{}: `{field}` must not contain {character:?} — it would change the meaning of the generated Traefik label{hint}",
                    span.line, span.col
                )
            }
        }
    }
}

impl std::error::Error for CodegenError {}

/// Generates one combined Compose document from `program`. Pure — no
/// I/O — mirroring `hl_parser::compose::compose`'s own by-value,
/// side-effect-free signature.
pub fn generate(program: ComposedProgram) -> Result<GeneratedProgram, CodegenError> {
    let mut services = IndexMap::new();
    let mut networks: IndexMap<String, doc::NetworkDoc> = IndexMap::new();
    let mut volumes: IndexMap<String, Option<()>> = IndexMap::new();

    for service in &program.services {
        let (service_doc, network_docs, named_volumes) =
            generate_service(service, &program.networks)?;
        for (net_name, net_doc) in network_docs {
            networks.entry(net_name).or_insert(net_doc);
        }
        for vol_name in named_volumes {
            volumes.entry(vol_name).or_insert(None);
        }
        services.insert(service.name.name.clone(), service_doc);
    }

    let compose_doc = doc::ComposeDoc {
        services,
        networks,
        volumes,
    };
    let yaml = serde_yaml::to_string(&compose_doc)
        .expect("ComposeDoc only contains strings/maps/numbers; serialization cannot fail");
    Ok(GeneratedProgram { yaml })
}

fn generate_service(
    service: &Service,
    declared_networks: &[Network],
) -> Result<(doc::ComposeServiceDoc, NetworkDocs, Vec<String>), CodegenError> {
    let name = &service.name.name;
    let fields = &service.fields;
    let bindings: HashMap<&str, &str> = HashMap::from([("name", name.as_str())]);

    let image_lit = fields
        .image
        .as_ref()
        .and_then(|i| i.reference.as_ref())
        .ok_or_else(|| CodegenError::MissingImage {
            service: name.clone(),
            span: service.span,
        })?;
    let image = interp::resolve(image_lit.text(), &bindings, image_lit.span())?;

    let container_name = fields
        .container_name
        .as_ref()
        .map(|lit| interp::resolve(lit.text(), &bindings, lit.span()))
        .transpose()?;

    let restart = fields
        .restart
        .as_ref()
        .and_then(|r| r.policy.as_ref())
        .map(|lit| interp::resolve(lit.text(), &bindings, lit.span()))
        .transpose()?;

    let mut environment = Vec::with_capacity(fields.env.entries.len());
    for e in &fields.env.entries {
        let key = interp::resolve(e.key.text(), &bindings, e.key.span())?;
        let value = interp::resolve(e.value.text(), &bindings, e.value.span())?;
        environment.push(format!("{key}={value}"));
    }

    let mut volume_entries = Vec::with_capacity(fields.volumes.entries.len());
    let mut named_volumes = Vec::new();
    for v in &fields.volumes.entries {
        let host = interp::resolve(v.host.text(), &bindings, v.host.span())?;
        let container = interp::resolve(v.container.text(), &bindings, v.container.span())?;
        if let Some(vol_name) = volume::classify_named_volume(&host) {
            named_volumes.push(vol_name.to_string());
        }
        volume_entries.push(format!("{host}:{container}"));
    }

    let (compose_networks, network_docs, docker_network) =
        resolve_networks(&fields.networks, declared_networks, name, service.span)?;

    let expose: Vec<serde_yaml::Value> = fields
        .expose
        .as_ref()
        .and_then(|e| e.port.as_ref())
        .map(raw::scalar_value)
        .into_iter()
        .collect();

    let labels = labels::compute(name, fields, docker_network.as_deref(), &bindings)?;

    let depends_on = fields.depends_on.iter().map(|r| r.name.clone()).collect();
    let dns = fields.dns.iter().map(|r| r.name.clone()).collect();

    let mut raw_map = IndexMap::new();
    for entry in &fields.raw.entries {
        let key = interp::resolve(entry.key.text(), &bindings, entry.key.span())?;
        raw_map.insert(key, raw::to_yaml(&entry.value, &bindings)?);
    }

    let mut service_doc = doc::ComposeServiceDoc {
        image: Some(image),
        container_name,
        restart,
        environment,
        volumes: volume_entries,
        networks: compose_networks,
        dns,
        expose,
        depends_on,
        labels,
        raw: raw_map,
    };
    // Required, not optional tidying: without it a `raw` key that
    // shadows a built-in field emits the key twice (#68).
    service_doc.apply_raw_overrides();

    Ok((service_doc, network_docs, named_volumes))
}

/// Resolves a service's `networks [x, ...]` references against the
/// program's top-level `network` declarations. Returns the Compose-level
/// network name list, the `(name, doc)` pairs to merge into the
/// program's top-level `networks:` section, and — if the referenced
/// networks name exactly one distinct external real name — that name,
/// for the `traefik.docker.network=` label.
///
/// `service_span` is only used for [`CodegenError::AmbiguousExternalNetwork`],
/// which is a property of the service's whole `networks` list rather
/// than of any one entry in it — there is no single offending
/// reference to point at. [`CodegenError::UnknownNetwork`] does have
/// one, and points at it (#70).
fn resolve_networks(
    refs: &[Reference],
    declared: &[Network],
    service_name: &str,
    service_span: Span,
) -> Result<(Vec<String>, NetworkDocs, Option<String>), CodegenError> {
    let mut compose_networks = Vec::with_capacity(refs.len());
    let mut docs = Vec::with_capacity(refs.len());
    let mut external_candidates = Vec::new();

    for r in refs {
        let decl = declared
            .iter()
            .find(|n| n.name.name == r.name)
            .ok_or_else(|| CodegenError::UnknownNetwork {
                service: service_name.to_string(),
                network: r.name.clone(),
                span: r.span,
            })?;
        compose_networks.push(decl.name.name.clone());
        let is_external = decl.external.is_some();
        let real_name = decl
            .real_name
            .as_ref()
            .map(|l| l.text().to_string())
            .unwrap_or_else(|| decl.name.name.clone());
        // By *distinct* real name (#69): naming one external network
        // more than once is not an ambiguity between it and itself,
        // it's one answer given twice. Composition already drops
        // repeated `networks` entries, so a duplicate can only reach
        // here from a caller that built a `ComposedProgram` some other
        // way, or from two declarations that differ in `hll` name but
        // resolve to the same real one — neither of which leaves
        // `traefik.docker.network` with an actual choice to make.
        if is_external && !external_candidates.contains(&real_name) {
            external_candidates.push(real_name.clone());
        }
        docs.push((
            decl.name.name.clone(),
            doc::NetworkDoc {
                name: decl.real_name.as_ref().map(|l| l.text().to_string()),
                external: is_external,
            },
        ));
    }

    let docker_network = match external_candidates.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        many => {
            return Err(CodegenError::AmbiguousExternalNetwork {
                service: service_name.to_string(),
                candidates: many.to_vec(),
                span: service_span,
            });
        }
    };

    Ok((compose_networks, docs, docker_network))
}

#[cfg(test)]
mod error_display_tests {
    use super::*;

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 3,
            col: 5,
        }
    }

    #[test]
    fn unknown_network_display() {
        let err = CodegenError::UnknownNetwork {
            service: "web".to_string(),
            network: "proxy".to_string(),
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: service `web` references undeclared network `proxy`"
        );
    }

    #[test]
    fn ambiguous_external_network_display() {
        let err = CodegenError::AmbiguousExternalNetwork {
            service: "web".to_string(),
            candidates: vec!["a".to_string(), "b".to_string()],
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: service `web` declares more than one external network (a, b) — which real network name should `traefik.docker.network` use?"
        );
    }

    #[test]
    fn unknown_interpolation_display() {
        let err = CodegenError::UnknownInterpolation {
            binding: "port".to_string(),
            span: span(),
        };
        assert_eq!(err.to_string(), "3:5: unknown interpolation `{{port}}`");
    }

    #[test]
    fn missing_image_display() {
        let err = CodegenError::MissingImage {
            service: "web".to_string(),
            span: span(),
        };
        assert_eq!(err.to_string(), "3:5: service `web` has no `image` set");
    }

    #[test]
    fn unsubstituted_parameter_display() {
        let err = CodegenError::UnsubstitutedParameter {
            param: "puid".to_string(),
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: template parameter `$puid` was never bound to an argument"
        );
    }

    #[test]
    fn unsafe_label_value_display() {
        let err = CodegenError::UnsafeLabelValue {
            field: "expose.host",
            character: '`',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: `expose.host` must not contain '`' — it would change the meaning of the generated Traefik label"
        );
    }

    /// A comma in `expose.entrypoint` — and only that pairing — gets the
    /// migration hint appended.
    #[test]
    fn comma_in_entrypoint_display_adds_a_list_hint() {
        let err = CodegenError::UnsafeLabelValue {
            field: "expose.entrypoint",
            character: ',',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: `expose.entrypoint` must not contain ',' — it would change the meaning of the generated Traefik label — `entrypoint` is a list, so write the entry points as separate items (`entrypoint web, websecure`) and let `hllc` join them"
        );
    }

    /// The hint is specific to the comma: another metacharacter in the
    /// same field has nothing to do with list syntax, so suggesting a
    /// list there would just be wrong.
    #[test]
    fn non_comma_in_entrypoint_display_has_no_list_hint() {
        let err = CodegenError::UnsafeLabelValue {
            field: "expose.entrypoint",
            character: '`',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: `expose.entrypoint` must not contain '`' — it would change the meaning of the generated Traefik label"
        );
    }

    /// And a comma in a *different* field doesn't get it either.
    #[test]
    fn comma_in_middleware_display_has_no_list_hint() {
        let err = CodegenError::UnsafeLabelValue {
            field: "middleware",
            character: ',',
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: `middleware` must not contain ',' — it would change the meaning of the generated Traefik label"
        );
    }
}
