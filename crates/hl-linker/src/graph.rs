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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use hl_parser::{
    ComposeError, Ident, Network, Service, SourceMap, Span, SymbolResolver, TemplateDecl, TopDecl,
    parse_in_file,
};

use crate::error::LinkError;
use crate::loader::FileLoader;
use crate::path::{normalize, resolve_relative};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ModuleId(usize);

#[derive(Default)]
struct Module {
    templates: HashMap<String, TemplateDecl>,
    networks: HashMap<String, Network>,
    aliases: HashMap<String, ModuleId>,
    /// Only ever populated for the entry module — see [`Graph::take_entry`].
    services: Vec<Service>,
    /// The entry module's own top-level `network` decls, in source
    /// order — distinct from `networks` above (a by-name map used for
    /// *qualified* `alias.name` lookups against any module, entry or
    /// not) since `compose_with_resolver` wants these as a plain,
    /// order-preserving `Vec`.
    entry_networks: Vec<Network>,
}

pub(crate) struct Graph {
    modules: Vec<Module>,
    /// Every module's path, interned as it was loaded, so the
    /// [`hl_parser::FileId`] stamped into that module's spans resolves
    /// back to a path a diagnostic can print (#75).
    files: SourceMap,
}

impl Graph {
    pub(crate) fn entry_scope(&self) -> ModuleId {
        ModuleId(0)
    }

    /// The path table every span in this graph resolves against.
    pub(crate) fn files(&self) -> &SourceMap {
        &self.files
    }

    /// Takes ownership of the entry module's own networks/services,
    /// leaving everything else (templates, by-name networks, aliases —
    /// for every module, entry included) untouched for
    /// [`SymbolResolver`] lookups during composition.
    pub(crate) fn take_entry(&mut self) -> (Vec<Network>, Vec<Service>) {
        let entry = &mut self.modules[0];
        (
            std::mem::take(&mut entry.entry_networks),
            std::mem::take(&mut entry.services),
        )
    }
}

pub(crate) fn build(entry: &Path, loader: &dyn FileLoader) -> Result<Graph, LinkError> {
    let mut modules: Vec<Module> = vec![Module::default()];
    // Grows alongside `modules`, one interned path per module, so a span
    // parsed out of a module can be traced back to the file it came
    // from long after composition has merged it into a service that
    // borrowed fields from several files at once (#75).
    let mut files = SourceMap::new();
    let mut path_to_id: HashMap<PathBuf, ModuleId> = HashMap::new();
    let mut queue: VecDeque<(ModuleId, PathBuf)> = VecDeque::new();

    let entry_id = ModuleId(0);
    let entry_path = normalize(entry);
    path_to_id.insert(entry_path.clone(), entry_id);
    queue.push_back((entry_id, entry_path));

    while let Some((id, path)) = queue.pop_front() {
        let source = loader.read(&path).map_err(|err| LinkError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?;
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
                    if is_entry {
                        module.services.push(*s);
                    }
                    // A non-entry file's own `service` decls are parsed
                    // but otherwise inert — nothing can reference a
                    // service across files, only templates/networks.
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
                    let resolved = resolve_relative(&path, u.path.text()).ok_or_else(|| {
                        LinkError::PathEscape {
                            path: path.clone(),
                            raw: u.path.text().to_string(),
                            span: u.path.span(),
                        }
                    })?;
                    let target_id = *path_to_id.entry(resolved.clone()).or_insert_with(|| {
                        let new_id = ModuleId(modules.len());
                        modules.push(Module::default());
                        queue.push_back((new_id, resolved));
                        new_id
                    });
                    alias_spans.insert(alias_name.clone(), u.alias.span);
                    module.aliases.insert(alias_name, target_id);
                }
            }
        }

        modules[id.0] = module;
    }

    Ok(Graph { modules, files })
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
            Some(q) => *self.modules[scope.0].aliases.get(&q.name).ok_or_else(|| {
                ComposeError::UnknownAlias {
                    alias: q.name.clone(),
                    span: q.span,
                }
            })?,
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

    fn resolve_defaults(&self, scope: ModuleId) -> Option<(ModuleId, &TemplateDecl)> {
        self.modules[scope.0]
            .templates
            .get("defaults")
            .map(|decl| (scope, decl))
    }

    fn resolve_qualified_network(
        &self,
        scope: ModuleId,
        qualifier: &Ident,
        name: &str,
        span: Span,
    ) -> Result<&Network, ComposeError> {
        let target = *self.modules[scope.0]
            .aliases
            .get(&qualifier.name)
            .ok_or_else(|| ComposeError::UnknownAlias {
                alias: qualifier.name.clone(),
                span: qualifier.span,
            })?;
        self.modules[target.0].networks.get(name).ok_or_else(|| {
            ComposeError::UnknownQualifiedNetwork {
                alias: qualifier.name.clone(),
                name: name.to_string(),
                span,
            }
        })
    }
}
