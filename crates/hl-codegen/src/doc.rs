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

/// Compose's own `healthcheck:` key (#153) — mirrors [`NetworkDoc`]'s
/// style, with `skip_serializing_if` on every field so an unset one is
/// simply omitted rather than emitted as `null`.
#[derive(Serialize, Default, PartialEq)]
pub(crate) struct HealthcheckDoc {
    /// Either a plain YAML string (Compose's shell form — a bare string
    /// is shorthand for `CMD-SHELL <string>`) or a YAML sequence
    /// (Compose's exec form, `["CMD", "pg_isready", "-U", "miniflux"]`),
    /// carried through in whichever shape `.hll` wrote it rather than
    /// normalizing one into the other — see
    /// [`hl_parser::HealthcheckTest`]'s doc for why the two aren't
    /// interchangeable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<serde_yaml_ng::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// A YAML number when written as one (`retries: 3`), carried through
    /// via [`crate::raw::scalar_value`] exactly like `expose.port` — see
    /// that call site's own doc for why this field skips
    /// `{{name}}`-interpolation the string fields above get.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<serde_yaml_ng::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_interval: Option<String>,
    /// Compose's `disable: true`. There's no `false` form to represent
    /// — [`hl_parser::Healthcheck::disable`] is bare-presence-only, so
    /// this is always emitted as `true` or omitted entirely, never
    /// `false`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disable: bool,
}

impl HealthcheckDoc {
    /// Whether this `healthcheck` set nothing at all — in which case
    /// [`ComposeServiceDoc::healthcheck`] stores `None` and the service
    /// gets no `healthcheck:` key, rather than an empty `{}` mapping.
    /// Mirrors [`VolumeDoc::is_empty`]; see that doc for why the empty
    /// case is checked explicitly instead of just serializing whatever
    /// came out.
    pub(crate) fn is_empty(&self) -> bool {
        *self == HealthcheckDoc::default()
    }
}

/// Compose's `depends_on:` key, in whichever of its two mutually
/// exclusive shapes this service's own entries need (#155). Compose
/// allows either a plain sequence of service names (the *short* form —
/// what `hll` has always emitted, "wait for the target container to
/// start") or a mapping of name to `{ condition: ... }` (the *long*
/// form — "wait for a named readiness condition"), but never a mix of
/// the two in one document.
///
/// `hllc` picks `Short` when none of a service's `depends_on` entries
/// carries an explicit `condition`, so every `depends_on [db]`/
/// `depends_on database` file — which is to say, every file written
/// before #155 — keeps emitting the exact YAML it always has, byte for
/// byte. It picks `Long` as soon as *any* entry does, since Compose's
/// long form is all-or-nothing per document: once one entry needs a
/// mapping value, every entry does, so [`crate::generate_depends_on`]
/// fills in Compose's own implicit default (`service_started`) for any
/// sibling entry that named no condition — the same "wait for container
/// start" behavior the short form always meant, just spelled the long
/// way.
#[derive(Serialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum DependsOnDoc {
    Short(Vec<String>),
    Long(IndexMap<String, DependsOnConditionDoc>),
}

impl Default for DependsOnDoc {
    /// The short, empty form — matching `Vec::default()`'s own
    /// "nothing set" meaning for every other list field on
    /// [`ComposeServiceDoc`], and what `skip_serializing_if` compares
    /// against via [`Self::is_empty`].
    fn default() -> Self {
        DependsOnDoc::Short(Vec::new())
    }
}

impl DependsOnDoc {
    /// Whether this `depends_on` set nothing at all — in which case the
    /// service gets no `depends_on:` key. Mirrors
    /// [`HealthcheckDoc::is_empty`]/[`VolumeDoc::is_empty`]; `Long` is
    /// never actually constructed empty (see
    /// [`crate::generate_depends_on`]), but the check costs nothing and
    /// keeps this type self-consistent regardless of who builds one.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            DependsOnDoc::Short(v) => v.is_empty(),
            DependsOnDoc::Long(m) => m.is_empty(),
        }
    }
}

#[derive(Serialize, PartialEq)]
pub(crate) struct DependsOnConditionDoc {
    pub condition: String,
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
    /// Compose's own `command:` key (#156) — either a plain YAML string
    /// (Compose's shell form) or a YAML sequence (Compose's exec form),
    /// carried through in whichever shape `.hll` wrote it, exactly
    /// mirroring [`HealthcheckDoc::test`] just below (see that field's
    /// own doc for why the two forms aren't normalized into one
    /// another). `None` when `.hll` sets no `command` field at all —
    /// unlike `healthcheck`, there's no "every sub-field unset" case to
    /// distinguish here, since `command` has no sub-fields of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<serde_yaml_ng::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    /// Compose's own `healthcheck:` key (#153). `None` both when
    /// `.hll` sets no `healthcheck` field at all and when it does but
    /// leaves every sub-field unset (`healthcheck {}`) — see
    /// [`HealthcheckDoc::is_empty`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthcheckDoc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<String>,
    /// `env_file` paths, carried through verbatim (#154). Always emitted
    /// as a list — even a single `env_file "one.env"` becomes a
    /// one-element `env_file:` list — rather than Compose's alternative
    /// bare-string shorthand, so the emitted shape doesn't depend on how
    /// many paths were written. Compose resolves each path relative to
    /// the compose file itself; that's the user's concern; `hllc` never
    /// inspects or rewrites a path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_file: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    /// `publish host -> container` entries, as Compose short-syntax
    /// `host:container` strings — one string per mapping, never a
    /// nested `{ target, published }` mapping.
    ///
    /// Typed `String` rather than [`serde_yaml_ng::Value`] (which
    /// `expose` below uses) precisely because a port *mapping* is never
    /// a number: the whole `53:53` is one scalar. YAML 1.1's
    /// sexagesimal rule would have read that as the integer 3233, which
    /// is the classic reason Compose's own docs tell you to quote port
    /// mappings — but the rule was dropped in YAML 1.2 and Compose's
    /// parser explicitly refuses to honor it ("Base 60 floats are a bad
    /// idea, were dropped in YAML 1.2, and are purposefully unsupported
    /// here" — `go-yaml/yaml` v3's `resolve.go`), so a plain scalar
    /// reaches Compose as the string it is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<serde_yaml_ng::Value>,
    /// See [`DependsOnDoc`]'s own doc for the short-vs-long shape switch
    /// (#155).
    #[serde(skip_serializing_if = "DependsOnDoc::is_empty")]
    pub depends_on: DependsOnDoc,
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
            command,
            restart,
            healthcheck,
            environment,
            env_file,
            volumes,
            networks,
            dns,
            ports,
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
        if raw.contains_key("command") {
            *command = None;
        }
        if raw.contains_key("restart") {
            *restart = None;
        }
        if raw.contains_key("healthcheck") {
            *healthcheck = None;
        }
        if raw.contains_key("environment") {
            environment.clear();
        }
        if raw.contains_key("env_file") {
            env_file.clear();
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
        if raw.contains_key("ports") {
            ports.clear();
        }
        if raw.contains_key("expose") {
            expose.clear();
        }
        if raw.contains_key("depends_on") {
            *depends_on = DependsOnDoc::default();
        }
        if raw.contains_key("labels") {
            labels.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HealthcheckDoc::is_empty`'s own documented invariant, checked
    /// directly against the struct rather than through
    /// [`crate::generate_healthcheck`] — that function already turns
    /// "every sub-field unset" into a bare `None` before a
    /// `HealthcheckDoc` is ever constructed (see its own doc), so this
    /// equality check is never actually reached with a truly-default
    /// value along that path. It stays load-bearing all the same: it's
    /// what [`ComposeServiceDoc::healthcheck`]'s `skip_serializing_if`
    /// would fall back on for any future caller that constructs a
    /// `HealthcheckDoc` some other way, exactly as
    /// [`VolumeDoc::is_empty`] does for `volumes`.
    #[test]
    fn default_healthcheck_doc_is_empty() {
        assert!(HealthcheckDoc::default().is_empty());
    }

    /// The mirror case: any one field set at all — here just
    /// `interval` — makes it non-empty.
    #[test]
    fn healthcheck_doc_with_a_field_set_is_not_empty() {
        let doc = HealthcheckDoc {
            interval: Some("10s".to_string()),
            ..HealthcheckDoc::default()
        };
        assert!(!doc.is_empty());
    }

    /// `disable: true` alone also counts as non-empty — it's a bare
    /// `bool`, not an `Option`, so it doesn't ride the same "any field
    /// is `Some`" shortcut the other five do.
    #[test]
    fn healthcheck_doc_with_only_disable_set_is_not_empty() {
        let doc = HealthcheckDoc {
            disable: true,
            ..HealthcheckDoc::default()
        };
        assert!(!doc.is_empty());
    }
}
