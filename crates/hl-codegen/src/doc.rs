//! The internal Compose-document IR that gets serialized to YAML.
//! `IndexMap` (not `HashMap`) throughout — Compose YAML key order should
//! be deterministic and readable, unlike hash-map iteration order.

use indexmap::IndexMap;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct ComposeDoc {
    pub services: IndexMap<String, ComposeServiceDoc>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub networks: IndexMap<String, NetworkDoc>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub volumes: IndexMap<String, Option<()>>,
}

#[derive(Serialize)]
pub(crate) struct NetworkDoc {
    /// The real underlying Docker network name, when it differs from
    /// this network's own hl-lang identifier (the map key it's stored
    /// under in [`ComposeDoc::networks`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
}

#[derive(Serialize, Default)]
pub(crate) struct ComposeServiceDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Unlike every other optional field here, always populated —
    /// codegen defaults it to the service's own name when `.hll` doesn't
    /// set one explicitly, matching real-world hand-written Compose
    /// files (see `hl_codegen::generate_service`).
    pub container_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<serde_yaml::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// `raw {}` entries — flattened so they land as sibling top-level
    /// service keys (e.g. `privileged: true`), matching `raw`'s own
    /// verbatim-passthrough design intent. `serde(flatten)` doesn't
    /// support `skip_serializing_if`, but an empty map flattens to zero
    /// extra keys anyway, so nothing extra is needed here.
    #[serde(flatten)]
    pub raw: IndexMap<String, serde_yaml::Value>,
}
