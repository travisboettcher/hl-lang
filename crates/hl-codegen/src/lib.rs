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
}

impl CodegenError {
    pub fn span(&self) -> Span {
        match self {
            CodegenError::UnknownNetwork { span, .. }
            | CodegenError::AmbiguousExternalNetwork { span, .. }
            | CodegenError::UnknownInterpolation { span, .. }
            | CodegenError::MissingImage { span, .. } => *span,
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

    let container_name = match fields.container_name.as_ref() {
        Some(lit) => interp::resolve(lit.text(), &bindings, lit.span())?,
        None => name.clone(),
    };

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

    let service_doc = doc::ComposeServiceDoc {
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

    Ok((service_doc, network_docs, named_volumes))
}

/// Resolves a service's `networks [x, ...]` references against the
/// program's top-level `network` declarations. Returns the Compose-level
/// network name list, the `(name, doc)` pairs to merge into the
/// program's top-level `networks:` section, and — if exactly one
/// referenced network is `external` — its real name, for the
/// `traefik.docker.network=` label.
fn resolve_networks(
    refs: &[Reference],
    declared: &[Network],
    service_name: &str,
    span: Span,
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
                span,
            })?;
        compose_networks.push(decl.name.name.clone());
        let is_external = decl.external.is_some();
        let real_name = decl
            .real_name
            .as_ref()
            .map(|l| l.text().to_string())
            .unwrap_or_else(|| decl.name.name.clone());
        if is_external {
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
                span,
            });
        }
    };

    Ok((compose_networks, docs, docker_network))
}
