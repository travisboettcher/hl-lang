//! Builds the module graph a whole `use` tree resolves to, then
//! implements [`SymbolResolver`] directly over it — `Scope = ModuleId`,
//! an opaque index into `Graph::modules`, exactly the "module identity"
//! [`SymbolResolver`]'s own doc anticipates.
//!
//! Loading is eager and cycle-safe by construction: every file is
//! memoized by its resolved path (see [`crate::path`]) the first time
//! it's reached, via any alias, from anywhere in the graph, so a
//! cyclic `use` graph (`A` uses `B`, `B` uses `A`) just means `A` and
//! `B` each end up with an alias pointing at the other's already-loaded
//! `ModuleId` — no special-casing needed, and no infinite loop, since a
//! module already in `path_to_id` is never re-queued.
//!
//! A module bundled into the compiler ([`crate::stdlib`]) joins the same
//! graph, memoized the same way, but through a second table keyed by
//! module name rather than by path. Two tables rather than one keyed by
//! some unspellable path is what makes a collision *unrepresentable*
//! instead of merely unlikely: whatever a user names a file, and however
//! a relative `use` spells it, that path can only ever land in
//! `path_to_id`, so it can neither be mistaken for a bundled module nor
//! shadow one.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use hl_parser::{
    ComposeError, Ident, Network, Service, SourceMap, Span, SymbolResolver, TemplateDecl, TopDecl,
    Volume, parse_in_file,
};

use crate::error::LinkError;
use crate::loader::FileLoader;
use crate::path::{normalize, resolve_relative};
use crate::stdlib::{self, Registry};
use crate::warning::LinkWarning;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ModuleId(usize);

/// Where one queued module's bytes come from — the two namespaces a
/// `use` can land in, kept apart as data rather than as a convention
/// about path spelling.
enum ModuleSource {
    /// A file the [`FileLoader`] reads, at an already-normalized path
    /// (see [`crate::path`]).
    User(PathBuf),
    /// A module compiled into this binary. It carries its own source
    /// because the registry lookup that found it already happened, at
    /// resolve time, where a miss still has a `use` span to point at —
    /// so nothing here has to handle a module that turns out not to
    /// exist.
    Std { name: String, source: &'static str },
}

impl ModuleSource {
    /// The path this module's spans resolve to: what a diagnostic
    /// prints, and what the graph's [`SourceMap`] interns.
    fn display_path(&self) -> PathBuf {
        match self {
            ModuleSource::User(path) => path.clone(),
            ModuleSource::Std { name, .. } => stdlib::display_path(name),
        }
    }

    /// This module's source text. Only the user half can fail, and it
    /// fails the way it always did.
    fn read(&self, loader: &dyn FileLoader) -> Result<String, LinkError> {
        match self {
            ModuleSource::User(path) => loader.read(path).map_err(|err| LinkError::Io {
                path: path.clone(),
                message: err.to_string(),
            }),
            ModuleSource::Std { source, .. } => Ok((*source).to_string()),
        }
    }
}

/// What a `use` path names, before anything is loaded.
enum Import {
    User(PathBuf),
    Std(String),
}

/// Resolves one `use` path, as written, against the module that wrote
/// it. `None` is a path that escapes its own tree — absolute, or
/// climbing out through `..` — which stays rejected in both namespaces.
///
/// The `std:` prefix is matched *before* relative resolution, which is
/// the whole of what makes the namespace unshadowable: a `std:` path
/// never reaches [`resolve_relative`], so no file can answer to it.
/// Inside a bundled module a relative path stays inside the standard
/// library ([`stdlib::resolve_sibling`]) — a compiled-in module has no
/// directory in the user's tree to be relative to.
fn resolve_import(from: &ModuleSource, raw: &str) -> Option<Import> {
    if let Some(name) = stdlib::strip_prefix(raw) {
        return Some(Import::Std(name.to_string()));
    }
    match from {
        ModuleSource::User(path) => resolve_relative(path, raw).map(Import::User),
        ModuleSource::Std { name, .. } => stdlib::resolve_sibling(name, raw).map(Import::Std),
    }
}

#[derive(Default)]
struct Module {
    templates: HashMap<String, TemplateDecl>,
    networks: HashMap<String, Network>,
    /// Every module's own top-level `volume` decls, by name — the table
    /// a *qualified* `volume alias.name -> "/path"` mount resolves
    /// against, exactly as `networks` above serves `networks
    /// [alias.name]`.
    volumes: HashMap<String, Volume>,
    aliases: HashMap<String, ModuleId>,
    /// Only ever populated for the entry module — see [`Graph::take_entry`].
    services: Vec<Service>,
    /// The entry module's own top-level `network` decls, in source
    /// order — distinct from `networks` above (a by-name map used for
    /// *qualified* `alias.name` lookups against any module, entry or
    /// not) since `compose_with_resolver` wants these as a plain,
    /// order-preserving `Vec`.
    entry_networks: Vec<Network>,
    /// The entry module's own top-level `volume` decls, in source order
    /// — distinct from `volumes` above for exactly the reason
    /// `entry_networks` is distinct from `networks`: that one is a
    /// by-name map for qualified lookups against any module, this one is
    /// the plain, order-preserving `Vec` `compose_with_resolver` wants.
    entry_volumes: Vec<Volume>,
}

pub(crate) struct Graph {
    modules: Vec<Module>,
    /// Every module's path, interned as it was loaded, so the
    /// [`hl_parser::FileId`] stamped into that module's spans resolves
    /// back to a path a diagnostic can print (#75).
    files: SourceMap,
    /// Non-fatal diagnostics collected while loading, in load order —
    /// the entry module first, then each imported module in the order it
    /// was reached. Loading never stops for one (#80).
    warnings: Vec<LinkWarning>,
}

impl Graph {
    pub(crate) fn entry_scope(&self) -> ModuleId {
        ModuleId(0)
    }

    /// The path table every span in this graph resolves against.
    pub(crate) fn files(&self) -> &SourceMap {
        &self.files
    }

    /// Takes the warnings collected while the graph was loaded, leaving
    /// the graph itself usable as a [`SymbolResolver`] for composition.
    pub(crate) fn take_warnings(&mut self) -> Vec<LinkWarning> {
        std::mem::take(&mut self.warnings)
    }

    /// Takes ownership of the entry module's own networks/volumes/
    /// services, leaving everything else (templates, by-name networks
    /// and volumes, aliases — for every module, entry included)
    /// untouched for [`SymbolResolver`] lookups during composition.
    pub(crate) fn take_entry(&mut self) -> (Vec<Network>, Vec<Volume>, Vec<Service>) {
        let entry = &mut self.modules[0];
        (
            std::mem::take(&mut entry.entry_networks),
            std::mem::take(&mut entry.entry_volumes),
            std::mem::take(&mut entry.services),
        )
    }
}

pub(crate) fn build(
    entry: &Path,
    loader: &dyn FileLoader,
    registry: Registry,
) -> Result<Graph, LinkError> {
    let mut modules: Vec<Module> = vec![Module::default()];
    // Grows alongside `modules`, one interned path per module, so a span
    // parsed out of a module can be traced back to the file it came
    // from long after composition has merged it into a service that
    // borrowed fields from several files at once (#75).
    let mut files = SourceMap::default();
    let mut path_to_id: HashMap<PathBuf, ModuleId> = HashMap::new();
    // The bundled namespace's half of the same memoization, keyed by
    // module name — see this module's own doc comment for why it's a
    // second table rather than a reserved corner of `path_to_id`.
    let mut std_to_id: HashMap<String, ModuleId> = HashMap::new();
    let mut queue: VecDeque<(ModuleId, ModuleSource)> = VecDeque::new();
    // What an imported file can declare that this stage then drops on
    // the floor: its own `service`s (#80). Not an error — the file is
    // still perfectly usable for the templates, networks, and volumes it
    // was imported for — so these accumulate here and ride out alongside
    // the finished graph.
    let mut warnings: Vec<LinkWarning> = Vec::new();
    // Every `template defaults` declaration seen, and whether any
    // service ended up invoking one. Whether to warn isn't knowable
    // until the whole graph is loaded, so each declaration records the
    // index its warning *would* have taken in load order, and the
    // warnings are spliced back in at those positions below — see
    // `LinkWarning::UnappliedDefaults`.
    let mut defaults_decls: Vec<(usize, Span)> = Vec::new();
    let mut defaults_invoked = false;

    let entry_id = ModuleId(0);
    let entry_path = normalize(entry);
    path_to_id.insert(entry_path.clone(), entry_id);
    queue.push_back((entry_id, ModuleSource::User(entry_path)));

    while let Some((id, module_source)) = queue.pop_front() {
        // The path this module answers to from here on: the file's own
        // for a user file, a `std:`-spelled stand-in for a bundled one,
        // which is what gives a span inside a bundled module somewhere
        // to point at.
        let path = module_source.display_path();
        let source = module_source.read(loader)?;
        let file = files.intern(path.clone());
        let program = parse_in_file(&source, file).map_err(|err| LinkError::Parse {
            path: path.clone(),
            source: err,
        })?;

        let is_entry = id == entry_id;
        let mut module = Module::default();
        let mut alias_spans: HashMap<String, Span> = HashMap::new();
        // `module.services` is only populated for the entry module, so
        // unlike networks/templates it can't double as the by-name table
        // a duplicate check needs — a non-entry file's redeclared
        // service is still a mistake worth reporting.
        let mut service_spans: HashMap<String, Span> = HashMap::new();

        for decl in program.decls {
            match decl {
                TopDecl::Network(n) => {
                    // Checked before either collection is touched:
                    // `entry_networks` is first-wins for anything reading
                    // the list while `networks` is last-wins for
                    // `alias.name` lookups, so a duplicate would leave
                    // bare and qualified references disagreeing about
                    // which declaration is the real one (#63).
                    if let Some(prev) = module.networks.get(&n.name.name) {
                        return Err(LinkError::compose(
                            ComposeError::DuplicateNetworkName {
                                name: n.name.name.clone(),
                                first: prev.name.span,
                                second: n.name.span,
                            },
                            &files,
                        ));
                    }
                    if is_entry {
                        module.entry_networks.push(n.clone());
                    }
                    module.networks.insert(n.name.name.clone(), n);
                }
                TopDecl::Volume(v) => {
                    // Checked before either collection is touched, for
                    // the same reason as `network` above: `entry_volumes`
                    // is first-wins while `volumes` is last-wins for
                    // `alias.name` lookups, so a duplicate would leave
                    // bare and qualified references disagreeing about
                    // which declaration is the real one (#63).
                    if let Some(prev) = module.volumes.get(&v.name.name) {
                        return Err(LinkError::compose(
                            ComposeError::DuplicateVolumeName {
                                name: v.name.name.clone(),
                                first: prev.name.span,
                                second: v.name.span,
                            },
                            &files,
                        ));
                    }
                    if is_entry {
                        module.entry_volumes.push(v.clone());
                    }
                    module.volumes.insert(v.name.name.clone(), v);
                }
                TopDecl::Service(s) => {
                    if let Some(&first) = service_spans.get(&s.name.name) {
                        return Err(LinkError::compose(
                            ComposeError::DuplicateServiceName {
                                name: s.name.name.clone(),
                                first,
                                second: s.name.span,
                            },
                            &files,
                        ));
                    }
                    service_spans.insert(s.name.name.clone(), s.name.span);
                    // Qualified or not: `with defaults` in the entry
                    // file and `with common.defaults` against an
                    // imported one both mean the migration to #260's
                    // explicit application has happened, so neither
                    // should warn.
                    if s.fields.with.iter().any(|inv| inv.name.name == "defaults") {
                        defaults_invoked = true;
                    }
                    if is_entry {
                        module.services.push(*s);
                    } else {
                        // A non-entry file's own `service` decls are
                        // parsed but otherwise inert — nothing can
                        // reference a service across files, only
                        // templates/networks/volumes. Dropping them is
                        // right; dropping them *quietly* is what left a
                        // user who split a service into a library file
                        // with no output and no diagnostic (#80).
                        warnings.push(LinkWarning::ImportedService {
                            service: s.name.name.clone(),
                            span: s.name.span,
                        });
                    }
                }
                TopDecl::Template(t) => {
                    if let Some(prev) = module.templates.get(&t.name.name) {
                        return Err(LinkError::compose(
                            ComposeError::DuplicateTemplateName {
                                name: t.name.name.clone(),
                                first: prev.name.span,
                                second: t.name.span,
                            },
                            &files,
                        ));
                    }
                    // #260 removed the implicit `defaults` tier, so a
                    // template with this name is now ordinary and
                    // applies only where a `with` names it. A file
                    // written against the old behavior still compiles —
                    // it just silently stops picking those fields up —
                    // so the declaration site is recorded here and
                    // warned about below if nothing invokes it.
                    if t.name.name == "defaults" {
                        defaults_decls.push((warnings.len(), t.name.span));
                    }
                    module.templates.insert(t.name.name.clone(), *t);
                }
                TopDecl::Use(u) => {
                    let alias_name = u.alias.name.clone();
                    if let Some(&first_span) = alias_spans.get(&alias_name) {
                        return Err(LinkError::DuplicateAlias {
                            path: path.clone(),
                            alias: alias_name,
                            first: first_span,
                            second: u.alias.span,
                        });
                    }
                    let import =
                        resolve_import(&module_source, u.path.text()).ok_or_else(|| {
                            LinkError::PathEscape {
                                path: path.clone(),
                                raw: u.path.text().to_string(),
                                span: u.path.span(),
                            }
                        })?;
                    let target_id = match import {
                        Import::User(resolved) => {
                            *path_to_id.entry(resolved.clone()).or_insert_with(|| {
                                let new_id = ModuleId(modules.len());
                                modules.push(Module::default());
                                queue.push_back((new_id, ModuleSource::User(resolved)));
                                new_id
                            })
                        }
                        Import::Std(name) => {
                            // Looked up here rather than at read time so
                            // the miss still has the `use` that asked
                            // for it in hand — a module name is worth
                            // nothing to a reader without the import
                            // that named it.
                            let bundled = stdlib::source(registry, &name).ok_or_else(|| {
                                LinkError::UnknownStdModule {
                                    path: path.clone(),
                                    name: name.clone(),
                                    available: stdlib::available(registry),
                                    span: u.path.span(),
                                }
                            })?;
                            *std_to_id.entry(name.clone()).or_insert_with(|| {
                                let new_id = ModuleId(modules.len());
                                modules.push(Module::default());
                                queue.push_back((
                                    new_id,
                                    ModuleSource::Std {
                                        name,
                                        source: bundled,
                                    },
                                ));
                                new_id
                            })
                        }
                    };
                    alias_spans.insert(alias_name.clone(), u.alias.span);
                    module.aliases.insert(alias_name, target_id);
                }
            }
        }

        modules[id.0] = module;
    }

    // One warning per declaration, and only when nothing applies any of
    // them: a file that has migrated names `defaults` in a `with` and
    // stays quiet, while one written against the old implicit behavior
    // gets told the fields it declares are no longer reaching anything.
    //
    // Spliced in back-to-front so each recorded index is still valid
    // when its own insert happens, which is what keeps the whole list in
    // load order (see `Graph::warnings`).
    if !defaults_invoked {
        for (at, span) in defaults_decls.into_iter().rev() {
            warnings.insert(at, LinkWarning::UnappliedDefaults { span });
        }
    }

    Ok(Graph {
        modules,
        files,
        warnings,
    })
}

impl SymbolResolver for Graph {
    type Scope = ModuleId;

    fn resolve_template(
        &self,
        scope: ModuleId,
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
    ) -> Result<(ModuleId, &TemplateDecl), ComposeError> {
        let target = match qualifier {
            Some(q) => self.alias_target(scope, q)?,
            None => scope,
        };
        self.modules[target.0]
            .templates
            .get(name)
            .map(|decl| (target, decl))
            .ok_or_else(|| ComposeError::UnknownTemplate {
                name: name.to_string(),
                span,
            })
    }

    fn resolve_qualified_network(
        &self,
        scope: ModuleId,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Network, ComposeError> {
        let target = self.alias_target(scope, qualifier)?;
        self.modules[target.0].networks.get(name).ok_or_else(|| {
            ComposeError::UnknownQualifiedNetwork {
                alias: qualifier.name.clone(),
                name: name.to_string(),
                span,
            }
        })
    }

    fn resolve_qualified_volume(
        &self,
        scope: ModuleId,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Volume, ComposeError> {
        let target = self.alias_target(scope, qualifier)?;
        self.modules[target.0].volumes.get(name).ok_or_else(|| {
            ComposeError::UnknownQualifiedVolume {
                alias: qualifier.name.clone(),
                name: name.to_string(),
                span,
            }
        })
    }
}

impl Graph {
    /// The module an `alias.` qualifier names, from `scope`'s own alias
    /// table — the first half of every qualified lookup, whatever kind
    /// of declaration the second half goes looking for.
    fn alias_target(&self, scope: ModuleId, qualifier: &Ident) -> Result<ModuleId, ComposeError> {
        self.modules[scope.0]
            .aliases
            .get(&qualifier.name)
            .copied()
            .ok_or_else(|| ComposeError::UnknownAlias {
                alias: qualifier.name.clone(),
                span: qualifier.span,
            })
    }
}
