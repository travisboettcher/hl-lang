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
    /// `None` for a volume whose declaration sets no options at all,
    /// which serializes as a bare `volume-name:` key with a null value —
    /// Compose's own idiom for "just create it with the defaults", and
    /// what this section emitted before top-level `volume` declarations
    /// existed. A declaration that *does* set something serializes the
    /// [`VolumeDoc`] instead.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub volumes: IndexMap<String, Option<VolumeDoc>>,
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

/// One entry in the top-level `volumes:` section, mirroring
/// [`NetworkDoc`] field for field where the two Compose sections agree
/// (`name`, `external`) and adding the two knobs only volumes have.
#[derive(Serialize, Default, PartialEq)]
pub(crate) struct VolumeDoc {
    /// The real underlying Docker volume name, when it differs from this
    /// volume's own hl-lang identifier (the map key it's stored under in
    /// [`ComposeDoc::volumes`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub driver_opts: IndexMap<String, String>,
}

impl VolumeDoc {
    /// Whether this declaration set no options at all — in which case
    /// [`ComposeDoc::volumes`] stores `None` and emits the bare
    /// `volume-name:` key instead of an empty `{}` mapping. Both are
    /// valid Compose, but the bare key is the idiomatic spelling and the
    /// one this section has always emitted.
    pub(crate) fn is_empty(&self) -> bool {
        *self == VolumeDoc::default()
    }
}

#[derive(Serialize, Default)]
pub(crate) struct ComposeServiceDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Only emitted when `.hll` sets `container_name` explicitly (#90).
    /// Compose's own default naming (`<project>_<service>_1`) is scoped
    /// per project and is what most people want; an explicit
    /// `container_name` forces one specific name across every stack it's
    /// used in, so it must opt in rather than default to the service's
    /// own name — that default reliably collided across independent
    /// stacks sharing a common service name (`db`, `broker`, ...), and
    /// Compose refuses to start the second container with the same name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
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
    pub expose: Vec<serde_yaml_ng::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// `raw {}` entries — flattened so they land as sibling top-level
    /// service keys (e.g. `privileged: true`), matching `raw`'s own
    /// verbatim-passthrough design intent. `serde(flatten)` doesn't
    /// support `skip_serializing_if`, but an empty map flattens to zero
    /// extra keys anyway, so nothing extra is needed here.
    ///
    /// A key here that also names one of the fields above would flatten
    /// into a *second* copy of that key in the same mapping — invalid
    /// YAML (#68). [`ComposeServiceDoc::apply_raw_overrides`] is what
    /// keeps that from happening, and every construction of this struct
    /// has to call it.
    #[serde(flatten)]
    pub raw: IndexMap<String, serde_yaml_ng::Value>,
}

impl ComposeServiceDoc {
    /// Clears every built-in field that a `raw {}` entry names, so the
    /// `raw` value is the only copy of that key left to serialize.
    ///
    /// Two keys spelled the same in one mapping is invalid YAML, and
    /// downstream tools disagree about it in the worst possible way —
    /// `docker compose config` rejects the document outright, while
    /// Python's `yaml.safe_load` accepts it and silently takes the last
    /// one (#68). So one of the two has to go, and `raw` is the one
    /// that wins: it's documented as the escape hatch for Compose keys
    /// `hll` has no dedicated field for *yet*, which means today's
    /// `raw { ports: [...] }` is tomorrow's collision with a built-in
    /// `ports` field. Overriding keeps that file compiling the day the
    /// built-in lands, making new built-in fields a purely additive
    /// change; rejecting the collision instead would make every one of
    /// them a breaking change.
    ///
    /// The trade is that `raw { labels: [...] }` drops the computed
    /// Traefik labels rather than merging with them — deliberate, since
    /// a `raw` value that arrives half-merged isn't verbatim
    /// passthrough anymore. The book calls this out.
    ///
    /// Only the service key itself is suppressed. Anything else codegen
    /// derives from the overridden field stays — notably the top-level
    /// `volumes:`/`networks:` declarations, which a `raw` replacement
    /// still needs if it names the same named volume or network, and
    /// which `raw`'s unparsed values give no way to re-derive.
    pub(crate) fn apply_raw_overrides(&mut self) {
        // Destructured exhaustively on purpose: adding a field to this
        // struct without giving it an override rule below stops
        // compiling here, rather than silently reintroducing #68's
        // duplicate key for that field.
        let Self {
            image,
            container_name,
            restart,
            environment,
            volumes,
            networks,
            dns,
            expose,
            depends_on,
            labels,
            raw,
        } = self;

        // Key names are the serialized ones, which for this struct are
        // its Rust field names — there is no `#[serde(rename)]` above.
        if raw.contains_key("image") {
            *image = None;
        }
        if raw.contains_key("container_name") {
            *container_name = None;
        }
        if raw.contains_key("restart") {
            *restart = None;
        }
        if raw.contains_key("environment") {
            environment.clear();
        }
        if raw.contains_key("volumes") {
            volumes.clear();
        }
        if raw.contains_key("networks") {
            networks.clear();
        }
        if raw.contains_key("dns") {
            dns.clear();
        }
        if raw.contains_key("expose") {
            expose.clear();
        }
        if raw.contains_key("depends_on") {
            depends_on.clear();
        }
        if raw.contains_key("labels") {
            labels.clear();
        }
    }
}
