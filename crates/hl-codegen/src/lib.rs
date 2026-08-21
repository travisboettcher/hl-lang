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
mod warning;

use std::collections::{HashMap, HashSet};
use std::fmt;

use hl_parser::{
    Command, ComposedProgram, DependsOnEntry, Healthcheck, HealthcheckTest, Network, Reference,
    Service, SourceMap, Span, Volume, VolumeHost, VolumeMap,
};
use indexmap::IndexMap;

pub use warning::CodegenWarning;

/// The result of running codegen on a [`ComposedProgram`]: one combined
/// Compose document, plus every non-fatal diagnostic raised while
/// producing it.
///
/// The warnings live here, on the stage's own success value, rather than
/// in a separate return channel — that is the whole of codegen's
/// non-fatal machinery (#80). `hllc` prints each one to stderr and leaves
/// its exit code alone.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedProgram {
    pub yaml: String,
    /// In declaration order. Empty for the overwhelmingly common case of
    /// a program with nothing dropped.
    pub warnings: Vec<CodegenWarning>,
}

/// `(network identifier, its Compose doc)` pairs to merge into the
/// program's top-level `networks:` section.
type NetworkDocs = Vec<(String, doc::NetworkDoc)>;

/// The one network name every program gets for free, whether or not it
/// declares one (#152): Compose itself creates a `default` network for
/// any project and attaches every service with no explicit `networks:`
/// key to it, so a same-file, multi-service `.hll` program — already one
/// Compose stack, one output document — gets the same behavior without
/// spelling out `network default {}` purely to name it. See
/// [`resolve_networks`] for the undeclared-reference fallback this
/// enables and [`generate`] for the auto-attach half.
const IMPLICIT_DEFAULT_NETWORK: &str = "default";

/// `(volume identifier, its Compose doc)` pairs to merge into the
/// program's top-level `volumes:` section. `None` means the declaration
/// set no options — see [`doc::ComposeDoc::volumes`].
type VolumeDocs = Vec<(String, Option<doc::VolumeDoc>)>;

/// An error raised while generating Compose YAML from a composed
/// program. Mirrors [`hl_parser::ParseError`]/[`hl_parser::ComposeError`]'s
/// existing span-carrying, no-recovery style.
#[derive(Debug, Clone, PartialEq)]
pub enum CodegenError {
    /// A service's `networks [x]` references a network with no matching
    /// top-level `network` declaration in the same program. A hard
    /// error, not silent implicit-network creation — every real network
    /// reference in the target homelab has a corresponding declaration.
    ///
    /// The one exception is `x == "default"` (#152): every program gets
    /// that name for free, so an undeclared `networks [default]` resolves
    /// to Compose's own implicit default network instead of reaching this
    /// variant. See `IMPLICIT_DEFAULT_NETWORK` in this crate.
    UnknownNetwork {
        service: String,
        network: String,
        span: Span,
    },
    /// A service's `volume name -> "/path"` entry names a *named*
    /// volume — an unquoted identifier on the host side, per
    /// [`hl_parser::VolumeHost`] — with no matching top-level `volume`
    /// declaration in the same program.
    ///
    /// The exact analogue of [`Self::UnknownNetwork`], and for the same
    /// reason (#60): a name there used to become a real Compose named
    /// volume on sight, deduplicated only by exact string equality, so
    /// `snycthing-config` for `syncthing-config` silently produced a
    /// *second*, empty volume rather than sharing the first — and two
    /// services that happened to write the same string were
    /// indistinguishable from two services deliberately sharing one
    /// volume. Requiring the declaration makes both cases say which one
    /// they mean. Bind-mount paths are unaffected: a quoted host side
    /// names a path on the host, not a Docker-managed volume, and Docker
    /// itself requires no declaration for one.
    UnknownVolume {
        service: String,
        volume: String,
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
    /// A service sets `middleware` or `expose.entrypoint` without an
    /// `expose.host` to hang a router off.
    ///
    /// Both fields only ever reach Traefik as labels *on* a router, and
    /// `expose.host` is what creates that router — so with no host there
    /// is nothing to attach them to and no label to emit. Until #80 that
    /// was handled by emitting nothing at all, which meant a service
    /// whose author forgot `as "host.example.com"` deployed with its
    /// authentication middleware quietly absent: valid output, wrong
    /// service, no diagnostic. A router-less `middleware` is never
    /// something a user can have meant, so it's an error rather than a
    /// warning — the one case of the four in #80 that is.
    ///
    /// `field` is the offending field's name (`middleware` or
    /// `expose.entrypoint`) and `span` points at its first entry, which
    /// is the line to either delete or pair with a host.
    RouterFieldWithoutHost {
        service: String,
        field: &'static str,
        span: Span,
    },
}

impl CodegenError {
    pub fn span(&self) -> Span {
        match self {
            CodegenError::UnknownNetwork { span, .. }
            | CodegenError::UnknownVolume { span, .. }
            | CodegenError::AmbiguousExternalNetwork { span, .. }
            | CodegenError::UnknownInterpolation { span, .. }
            | CodegenError::MissingImage { span, .. }
            | CodegenError::UnsubstitutedParameter { span, .. }
            | CodegenError::UnsafeLabelValue { span, .. }
            | CodegenError::RouterFieldWithoutHost { span, .. } => *span,
        }
    }

    /// Renders this error with each location it mentions resolved
    /// against `files` — `path:line:col` instead of a bare `line:col`.
    ///
    /// Codegen runs on an already-composed program, whose fields may
    /// have come from any file in the `use` graph: a `{{binding}}` typo
    /// inside an imported template used to report a line number in a
    /// file the user never opened, with nothing to say which one (#75).
    /// The map to pass is [`ComposedProgram::files`], which
    /// `hl_linker::link` fills in.
    pub fn display<'a>(&'a self, files: &'a SourceMap) -> impl fmt::Display + 'a {
        DisplayCodegenError {
            error: self,
            files: Some(files),
        }
    }

    /// The one implementation behind both [`Self::display`] and the
    /// [`Display`](fmt::Display) impl — every location goes through
    /// [`Span::locate`], so the two renderings can't drift apart.
    fn write(&self, f: &mut fmt::Formatter<'_>, files: Option<&SourceMap>) -> fmt::Result {
        let at = self.span().locate(files);
        match self {
            CodegenError::UnknownNetwork {
                service, network, ..
            } => write!(
                f,
                "{at}: service `{service}` references undeclared network `{network}`"
            ),
            CodegenError::UnknownVolume {
                service, volume, ..
            } => write!(
                f,
                "{at}: service `{service}` references undeclared volume `{volume}`"
            ),
            CodegenError::AmbiguousExternalNetwork {
                service,
                candidates,
                ..
            } => write!(
                f,
                "{at}: service `{service}` declares more than one external network ({}) — which real network name should `traefik.docker.network` use?",
                candidates.join(", ")
            ),
            CodegenError::UnknownInterpolation { binding, .. } => {
                write!(f, "{at}: unknown interpolation `{{{{{binding}}}}}`")
            }
            CodegenError::MissingImage { service, .. } => {
                write!(f, "{at}: service `{service}` has no `image` set")
            }
            CodegenError::UnsubstitutedParameter { param, .. } => write!(
                f,
                "{at}: template parameter `${param}` was never bound to an argument"
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
                    "{at}: `{field}` must not contain {character:?} — it would change the meaning of the generated Traefik label{hint}"
                )
            }
            CodegenError::RouterFieldWithoutHost { service, field, .. } => write!(
                f,
                "{at}: service `{service}` sets `{field}` but has no `expose.host`, so there is no Traefik router to attach it to — add a host (`expose <port> as \"{service}.example.com\"`) or drop the `{field}`"
            ),
        }
    }
}

/// [`CodegenError::display`]'s return type: the error plus the map its
/// spans resolve against.
struct DisplayCodegenError<'a> {
    error: &'a CodegenError,
    files: Option<&'a SourceMap>,
}

impl fmt::Display for DisplayCodegenError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.write(f, self.files)
    }
}

impl fmt::Display for CodegenError {
    /// Renders every location as a bare `line:col`, with no file — all
    /// a caller that composed a lone [`hl_parser::Program`] could say
    /// anyway. A caller that has a [`SourceMap`] (the CLI, from
    /// [`ComposedProgram::files`]) wants [`CodegenError::display`]
    /// instead.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, None)
    }
}

impl std::error::Error for CodegenError {}

/// Generates one combined Compose document from `program`. Pure — no
/// I/O — mirroring `hl_parser::compose::compose`'s own by-value,
/// side-effect-free signature.
///
/// Any [`CodegenWarning`] raised on the way rides out on the returned
/// [`GeneratedProgram`]; nothing here stops for one.
pub fn generate(program: ComposedProgram) -> Result<GeneratedProgram, CodegenError> {
    let mut services = IndexMap::new();
    let mut networks: IndexMap<String, doc::NetworkDoc> = IndexMap::new();
    let mut volumes: IndexMap<String, Option<doc::VolumeDoc>> = IndexMap::new();
    let mut referenced_networks: HashSet<&str> = HashSet::new();

    // Two or more `service` decls in one program are, by construction,
    // one Compose stack meant to talk to each other — that's the whole
    // premise of #152. A lone service gets no auto-attach: Compose's own
    // implicit default network already covers it for free, and emitting
    // nothing there is correct, matching the pre-#152 behavior for every
    // single-service program that exists today.
    let auto_attach_default = program.services.len() >= 2;

    for service in &program.services {
        let (service_doc, network_docs, volume_docs) = generate_service(
            service,
            &program.networks,
            &program.volumes,
            auto_attach_default,
        )?;
        for (net_name, net_doc) in network_docs {
            networks.entry(net_name).or_insert(net_doc);
        }
        for (vol_name, vol_doc) in volume_docs {
            volumes.entry(vol_name).or_insert(vol_doc);
        }
        for r in &service.fields.networks {
            referenced_networks.insert(r.name.as_str());
        }
        services.insert(service.name.name.clone(), service_doc);
    }
    // Auto-attach reaches every service's *generated* `networks:` list
    // without going through a `Reference` in `service.fields.networks`,
    // so the loop above never sees it. Feeding it in here too keeps the
    // `UnusedNetwork` check below honest: an explicitly declared `network
    // default { ... }` in a multi-service program is referenced by every
    // service, same as if each had written `networks [default]` by hand,
    // and must not warn (#152).
    if auto_attach_default {
        referenced_networks.insert(IMPLICIT_DEFAULT_NETWORK);
    }

    // `networks:` is assembled from what services reference, never from
    // the declarations themselves, so a declaration nothing reaches is
    // simply absent from the output — indistinguishable from having
    // forgotten to declare it (#80). Checked against the *references*
    // rather than against `networks` above so the diagnostic survives a
    // future change to how the docs are keyed.
    let warnings = program
        .networks
        .iter()
        .filter(|decl| !referenced_networks.contains(decl.name.name.as_str()))
        .map(|decl| CodegenWarning::UnusedNetwork {
            network: decl.name.name.clone(),
            span: decl.name.span,
        })
        .collect();

    let compose_doc = doc::ComposeDoc {
        services,
        networks,
        volumes,
    };
    let yaml = serde_yaml_ng::to_string(&compose_doc)
        .expect("ComposeDoc only contains strings/maps/numbers; serialization cannot fail");
    Ok(GeneratedProgram { yaml, warnings })
}

fn generate_service(
    service: &Service,
    declared_networks: &[Network],
    declared_volumes: &[Volume],
    auto_attach_default: bool,
) -> Result<(doc::ComposeServiceDoc, NetworkDocs, VolumeDocs), CodegenError> {
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

    let command = generate_command(fields.command.as_ref(), &bindings)?;

    let restart = fields
        .restart
        .as_ref()
        .and_then(|r| r.policy.as_ref())
        .map(|lit| interp::resolve(lit.text(), &bindings, lit.span()))
        .transpose()?;

    let healthcheck = generate_healthcheck(fields.healthcheck.as_ref(), &bindings)?;

    let mut environment = Vec::with_capacity(fields.env.entries.len());
    for e in &fields.env.entries {
        let key = interp::resolve(e.key.text(), &bindings, e.key.span())?;
        let value = interp::resolve(e.value.text(), &bindings, e.value.span())?;
        environment.push(format!("{key}={value}"));
    }

    let (volume_entries, volume_docs) =
        resolve_volumes(&fields.volumes, declared_volumes, name, &bindings)?;

    // Compose short syntax, `"host:container"` — quoted, since YAML
    // reads a bare `8096:8096` as a sexagesimal integer rather than as a
    // port mapping. A protocol suffix (`publish 53 -> "53/udp"`) rides
    // along on the container half untouched, which is where Compose's
    // own short syntax puts it.
    let mut publish_entries = Vec::with_capacity(fields.publish.entries.len());
    for p in &fields.publish.entries {
        let host = interp::resolve(p.host.text(), &bindings, p.host.span())?;
        let container = interp::resolve(p.container.text(), &bindings, p.container.span())?;
        publish_entries.push(format!("{host}:{container}"));
    }

    let (compose_networks, network_docs, docker_network) = resolve_networks(
        &fields.networks,
        declared_networks,
        name,
        service.span,
        auto_attach_default,
    )?;

    let expose: Vec<serde_yaml_ng::Value> = fields
        .expose
        .as_ref()
        .and_then(|e| e.port.as_ref())
        .map(raw::scalar_value)
        .into_iter()
        .collect();

    let labels = labels::compute(name, fields, docker_network.as_deref(), &bindings)?;

    let depends_on = generate_depends_on(&fields.depends_on);
    let dns = fields.dns.iter().map(|r| r.name.clone()).collect();
    // Paths, carried through verbatim — never resolved against `bindings`
    // (matching `dns`/`middleware`/`depends_on`/`networks` just above:
    // none of `hll`'s reference-list fields interpolate `{{name}}`) and
    // never inspected for existence, since Compose itself resolves each
    // one relative to the compose file at deploy time, not `hllc` at
    // compile time (#154).
    let env_file = fields.env_file.iter().map(|r| r.name.clone()).collect();

    let mut raw_map = IndexMap::new();
    for entry in &fields.raw.entries {
        let key = interp::resolve(entry.key.text(), &bindings, entry.key.span())?;
        raw_map.insert(key, raw::to_yaml(&entry.value, &bindings)?);
    }

    let mut service_doc = doc::ComposeServiceDoc {
        image: Some(image),
        container_name,
        command,
        restart,
        healthcheck,
        environment,
        env_file,
        volumes: volume_entries,
        networks: compose_networks,
        dns,
        ports: publish_entries,
        expose,
        depends_on,
        labels,
        raw: raw_map,
    };
    // Required, not optional tidying: without it a `raw` key that
    // shadows a built-in field emits the key twice (#68).
    service_doc.apply_raw_overrides();

    Ok((service_doc, network_docs, volume_docs))
}

/// Builds a service's `depends_on:` doc from its parsed
/// [`DependsOnEntry`] list (#155) — see [`doc::DependsOnDoc`]'s own doc
/// for the short-vs-long shape switch this picks between. Deliberately
/// never `{{name}}`-interpolated, matching every other reference-list
/// field (`middleware`/`networks`/`dns`/`env_file`): a `depends_on`
/// entry names a same-file sibling service, not free text, so there is
/// nothing in it a binding could ever apply to.
///
/// Neither this function nor anything upstream of it warns when a
/// `service_healthy` entry targets a service with no `hll`-level
/// `healthcheck` field — deliberately. A Docker image can bake its own
/// `HEALTHCHECK` into its Dockerfile, entirely outside anything an
/// `.hll` file declares, so "no `healthcheck` field on the target
/// service" is not evidence the condition is meaningless; `hllc` has no
/// way to see an image's own healthcheck, and guessing wrong here would
/// be worse than saying nothing.
fn generate_depends_on(entries: &[DependsOnEntry]) -> doc::DependsOnDoc {
    if entries.iter().all(|e| e.condition.is_none()) {
        return doc::DependsOnDoc::Short(
            entries.iter().map(|e| e.reference.name.clone()).collect(),
        );
    }
    let mut long = IndexMap::new();
    for entry in entries {
        // An entry with no explicit condition still needs a mapping
        // value once the document has committed to the long form —
        // filled in with Compose's own implicit default via
        // `effective_condition`, which is exactly what the short form
        // always meant.
        long.insert(
            entry.reference.name.clone(),
            doc::DependsOnConditionDoc {
                condition: entry.effective_condition().compose_value().to_string(),
            },
        );
    }
    doc::DependsOnDoc::Long(long)
}

/// Builds a service's `command:` value from its parsed [`Command`]
/// field (#156), `{{name}}`-interpolating each literal exactly like
/// [`generate_healthcheck`]'s own `test` case just below — the two
/// share the same shell-vs-exec [`Command`]/[`HealthcheckTest`] shape,
/// so this function mirrors that one's `test` arm closely rather than
/// factoring out a shared helper for two call sites. Returns `None`
/// when `.hll` sets no `command` field at all — unlike `healthcheck`,
/// there's no "every sub-field unset" case to distinguish here, since
/// `command` has no sub-fields of its own to leave unset.
fn generate_command(
    command: Option<&Command>,
    bindings: &HashMap<&str, &str>,
) -> Result<Option<serde_yaml_ng::Value>, CodegenError> {
    match command {
        Some(Command::Shell(lit)) => Ok(Some(serde_yaml_ng::Value::String(interp::resolve(
            lit.text(),
            bindings,
            lit.span(),
        )?))),
        Some(Command::Exec(items, _)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(serde_yaml_ng::Value::String(interp::resolve(
                    item.text(),
                    bindings,
                    item.span(),
                )?));
            }
            Ok(Some(serde_yaml_ng::Value::Sequence(out)))
        }
        None => Ok(None),
    }
}

/// Builds a service's `healthcheck:` doc from its parsed
/// [`Healthcheck`], `{{name}}`-interpolating every string sub-field the
/// same way `restart.policy`/`container_name` are above — a `test`
/// command or a duration string is exactly as likely to want the
/// service's own name spliced in as those are. `retries` is the one
/// exception, handled via [`raw::scalar_value`] instead so a bare
/// `retries: 3` reaches Compose as the YAML integer it was written as,
/// matching `expose.port`'s own precedent just above in
/// [`generate_service`] for the same reason: a number is a number, not
/// text to interpolate into.
///
/// Returns `None` both when `hc` is `None` (no `healthcheck` field at
/// all) and when every sub-field it holds is unset (`healthcheck {}`) —
/// [`doc::HealthcheckDoc::is_empty`] is what tells those two cases
/// apart from "something was actually set."
fn generate_healthcheck(
    hc: Option<&Healthcheck>,
    bindings: &HashMap<&str, &str>,
) -> Result<Option<doc::HealthcheckDoc>, CodegenError> {
    let Some(hc) = hc else {
        return Ok(None);
    };

    let test =
        match &hc.test {
            Some(HealthcheckTest::Shell(lit)) => Some(serde_yaml_ng::Value::String(
                interp::resolve(lit.text(), bindings, lit.span())?,
            )),
            Some(HealthcheckTest::Exec(items, _)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(serde_yaml_ng::Value::String(interp::resolve(
                        item.text(),
                        bindings,
                        item.span(),
                    )?));
                }
                Some(serde_yaml_ng::Value::Sequence(out))
            }
            None => None,
        };
    let interval = hc
        .interval
        .as_ref()
        .map(|lit| interp::resolve(lit.text(), bindings, lit.span()))
        .transpose()?;
    let timeout = hc
        .timeout
        .as_ref()
        .map(|lit| interp::resolve(lit.text(), bindings, lit.span()))
        .transpose()?;
    let retries = hc.retries.as_ref().map(raw::scalar_value);
    let start_period = hc
        .start_period
        .as_ref()
        .map(|lit| interp::resolve(lit.text(), bindings, lit.span()))
        .transpose()?;
    let start_interval = hc
        .start_interval
        .as_ref()
        .map(|lit| interp::resolve(lit.text(), bindings, lit.span()))
        .transpose()?;

    let doc = doc::HealthcheckDoc {
        test,
        interval,
        timeout,
        retries,
        start_period,
        start_interval,
        disable: hc.disable.is_some(),
    };
    Ok(if doc.is_empty() { None } else { Some(doc) })
}

/// Resolves a service's `volume` entries into the Compose-level
/// `host:container` mount strings, plus the `(name, doc)` pairs to merge
/// into the program's top-level `volumes:` section.
///
/// The direct counterpart of [`resolve_networks`], differing only in
/// that not every entry is a reference: which entries are is settled by
/// then, in the parser, by which token the host side was written as (see
/// [`hl_parser::VolumeHost`]). A [`VolumeHost::Named`] resolves against
/// `declared` exactly as a `networks [x]` entry resolves against the
/// program's `network` declarations; a [`VolumeHost::BindMount`] passes
/// through untouched and contributes nothing to `volumes:`, since Docker
/// itself requires no declaration for a host path.
fn resolve_volumes(
    volumes: &VolumeMap,
    declared: &[Volume],
    service_name: &str,
    bindings: &HashMap<&str, &str>,
) -> Result<(Vec<String>, VolumeDocs), CodegenError> {
    let mut entries = Vec::with_capacity(volumes.entries.len());
    let mut docs = Vec::new();

    for v in &volumes.entries {
        // Host before container, so an entry with a problem on both
        // sides reports the left one — the order the user reads them in.
        let host = match &v.host {
            // A bind-mount path is an ordinary value and interpolates
            // like every other one (`volume "/srv/{{name}}" -> "/data"`).
            VolumeHost::BindMount(lit) => interp::resolve(lit.text(), bindings, lit.span())?,
            VolumeHost::Named(r) => {
                let decl = declared
                    .iter()
                    .find(|d| d.name.name == r.name)
                    .ok_or_else(|| {
                        CodegenError::UnknownVolume {
                            service: service_name.to_string(),
                            volume: r.name.clone(),
                            // The offending reference itself, not the
                            // enclosing service — same choice #70 made for
                            // `UnknownNetwork`.
                            span: r.span,
                        }
                    })?;
                let vol_doc = doc::VolumeDoc {
                    name: decl.real_name.as_ref().map(|l| l.text().to_string()),
                    external: decl.external.is_some(),
                    driver: decl.driver.as_ref().map(|l| l.text().to_string()),
                    driver_opts: decl
                        .driver_opts
                        .iter()
                        .map(|o| (o.key.text().to_string(), o.value.text().to_string()))
                        .collect(),
                };
                docs.push((
                    decl.name.name.clone(),
                    (!vol_doc.is_empty()).then_some(vol_doc),
                ));
                decl.name.name.clone()
            }
        };
        let container = interp::resolve(v.container.text(), bindings, v.container.span())?;
        // `:ro` only when the entry's `{ read_only }` body (#158) set the
        // flag — Compose short syntax has no third segment at all for
        // the (overwhelmingly common) unset case, so this must not
        // append an empty `:` suffix or a `:rw` one; either would change
        // output for every `volume` entry ever written before this
        // field existed.
        if v.read_only {
            entries.push(format!("{host}:{container}:ro"));
        } else {
            entries.push(format!("{host}:{container}"));
        }
    }

    Ok((entries, docs))
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
///
/// `auto_attach_default` is the multi-service half of #152: when set,
/// [`IMPLICIT_DEFAULT_NETWORK`] is appended to the returned list if it
/// isn't there already, exactly as if the service had written `networks
/// [default]` itself. Appended, not prepended, so explicitly named
/// networks keep their source order and the auto-attached one always
/// sorts last; checked for first so an explicit `networks [default]`
/// doesn't end up listed twice.
fn resolve_networks(
    refs: &[Reference],
    declared: &[Network],
    service_name: &str,
    service_span: Span,
    auto_attach_default: bool,
) -> Result<(Vec<String>, NetworkDocs, Option<String>), CodegenError> {
    let mut compose_networks = Vec::with_capacity(refs.len() + 1);
    let mut docs = Vec::with_capacity(refs.len());
    let mut external_candidates = Vec::new();

    for r in refs {
        match declared.iter().find(|n| n.name.name == r.name) {
            Some(decl) => push_declared_network(
                decl,
                &mut compose_networks,
                &mut docs,
                &mut external_candidates,
            ),
            // The one name every program has whether or not it's
            // declared (#152): Compose defines `default` itself, so an
            // otherwise-unknown reference to exactly that name resolves
            // to it instead of erroring, and — since there's no
            // declaration to draw a `NetworkDoc` from — contributes
            // nothing to `docs`, leaving the top-level `networks:`
            // section to say nothing about it, same as Compose's own
            // output would.
            None if r.name == IMPLICIT_DEFAULT_NETWORK => {
                compose_networks.push(IMPLICIT_DEFAULT_NETWORK.to_string());
            }
            None => {
                return Err(CodegenError::UnknownNetwork {
                    service: service_name.to_string(),
                    network: r.name.clone(),
                    span: r.span,
                });
            }
        }
    }

    // The auto-attach half of #152: every service in a multi-service
    // program is implicitly on `default` in addition to whatever it
    // named explicitly. Guarded on not already being present so a
    // service that writes `networks [default]` itself ends up with
    // exactly one entry, not two — auto-attach is a fallback for
    // services that said nothing, not a second copy for services that
    // already said it themselves.
    if auto_attach_default
        && !compose_networks
            .iter()
            .any(|n| n == IMPLICIT_DEFAULT_NETWORK)
    {
        match declared
            .iter()
            .find(|n| n.name.name == IMPLICIT_DEFAULT_NETWORK)
        {
            // An explicit `network default { ... }` still wins: its
            // `external`/`name` settings are honored exactly as any
            // other declared network's would be, including
            // participating in the `traefik.docker.network=` /
            // `AmbiguousExternalNetwork` logic below when it's
            // `external`.
            Some(decl) => push_declared_network(
                decl,
                &mut compose_networks,
                &mut docs,
                &mut external_candidates,
            ),
            None => compose_networks.push(IMPLICIT_DEFAULT_NETWORK.to_string()),
        }
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

/// The shared body of resolving one *declared* network reference,
/// factored out of [`resolve_networks`] so its explicit-`refs` loop and
/// its `auto_attach_default` fallback — which both need to resolve
/// `default` against an actual declaration when one exists — can't drift
/// apart.
fn push_declared_network(
    decl: &Network,
    compose_networks: &mut Vec<String>,
    docs: &mut NetworkDocs,
    external_candidates: &mut Vec<String>,
) {
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

#[cfg(test)]
mod error_display_tests {
    use super::*;
    use hl_parser::FileId;

    fn span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 3,
            col: 5,
            file: FileId::ANONYMOUS,
        }
    }

    /// A composed service's fields can come from any file in the `use`
    /// graph, so codegen's own diagnostics resolve each span against the
    /// program's [`SourceMap`] rather than reporting a bare `line:col`
    /// in a file the user was never told about (#75).
    #[test]
    fn display_with_a_source_map_names_the_file() {
        let mut files = SourceMap::default();
        let lib = files.intern("shared/templates.hll");
        let err = CodegenError::UnknownInterpolation {
            binding: "nmae".to_string(),
            span: Span {
                start: 0,
                end: 0,
                line: 4,
                col: 22,
                file: lib,
            },
        };
        assert_eq!(
            err.display(&files).to_string(),
            "shared/templates.hll:4:22: unknown interpolation `{{nmae}}`"
        );
        // The bare `Display` is unchanged, for a caller with no map.
        assert_eq!(err.to_string(), "4:22: unknown interpolation `{{nmae}}`");
    }

    /// A span whose file the map doesn't know (an anonymous one from
    /// `hl_parser::parse`, say) still renders, just without a path.
    #[test]
    fn display_with_a_source_map_falls_back_for_anonymous_spans() {
        let mut files = SourceMap::default();
        files.intern("entry.hll");
        let err = CodegenError::MissingImage {
            service: "web".to_string(),
            span: span(),
        };
        assert_eq!(
            err.display(&files).to_string(),
            "3:5: service `web` has no `image` set"
        );
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

    /// #60: worded identically to `UnknownNetwork`, and resolved
    /// through the same [`Span::locate`] path, so the two read as one
    /// family of diagnostic rather than two.
    #[test]
    fn unknown_volume_display() {
        let err = CodegenError::UnknownVolume {
            service: "syncthing".to_string(),
            volume: "snycthing-config".to_string(),
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: service `syncthing` references undeclared volume `snycthing-config`"
        );
    }

    /// And, like every other codegen diagnostic, it names the file when
    /// the caller has a [`SourceMap`] to resolve against (#75).
    #[test]
    fn unknown_volume_display_with_a_source_map_names_the_file() {
        let mut files = SourceMap::default();
        let entry = files.intern("services/syncthing.hll");
        let err = CodegenError::UnknownVolume {
            service: "syncthing".to_string(),
            volume: "snycthing-config".to_string(),
            span: Span {
                start: 0,
                end: 0,
                line: 6,
                col: 10,
                file: entry,
            },
        };
        assert_eq!(
            err.display(&files).to_string(),
            "services/syncthing.hll:6:10: service `syncthing` references undeclared volume `snycthing-config`"
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

    /// #80: the error that replaced the old silent drop names the field,
    /// the service, and the fix — the missing `expose.host` is not
    /// something the service body shows on its face.
    #[test]
    fn router_field_without_host_display() {
        let err = CodegenError::RouterFieldWithoutHost {
            service: "web".to_string(),
            field: "middleware",
            span: span(),
        };
        assert_eq!(
            err.to_string(),
            "3:5: service `web` sets `middleware` but has no `expose.host`, so there is no \
             Traefik router to attach it to — add a host (`expose <port> as \
             \"web.example.com\"`) or drop the `middleware`"
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
