# hll design

`hll` (pronounced "hell" — short for **H**ome**L**ab **L**anguage) is a
small declarative DSL that transpiles to Docker Compose YAML plus Traefik
labels. It is a transpiler, not an interpreter — no evaluation, no closures,
no runtime. This document is the language's spec: grammar, semantics, and
worked examples. It's the source of truth the lexer, parser, and codegen
implementations are built against. Source files use the `.hll` extension;
the CLI binary is `hllc`.

## Motivation

Standing up a new homelab service usually means rewriting a near-identical
Docker Compose service block plus Traefik labels: image, port, subdomain,
volume, restart policy, sometimes Authentik forward-auth. `hl-lang` removes
that repetition by compiling a compact declaration down to the Compose YAML
and Traefik labels that would otherwise be hand-written.

## Design principle: generic core, specific templates

The compiler's built-in schema stays small and generically
Docker-Compose/Traefik-shaped — it has no knowledge of any particular
homelab's conventions (specific auth providers, domain names, PUID/PGID
values, etc.). Anything that's actually about *one* homelab belongs in
`template` files that get imported, not in the compiler itself. The test to
apply when considering a new built-in: "would this make sense to someone
else's homelab with none of my infra?" If not, it's a template, not a
grammar feature.

## Lexical grammar

```
IDENT   ::= [A-Za-z_][A-Za-z0-9_-]*
NUMBER  ::= [0-9]+
STRING  ::= '"' [^"\n]* '"'
COMMENT ::= '#' [^\n]*        # to end of line; not part of the token stream

Reserved (not usable as IDENT): "template"
Punctuation: { } [ ] ( ) : = -> , .
```

- `template` is the *only* reserved word in the entire language. Everything
  else that looks like a keyword (`service`, `network`, `image`, `volume`,
  `env`, `restart`, `expose`, `middleware`, `depends_on`, `networks`, `dns`,
  `with`, `as`, `external`, `use`, `raw`, `defaults`, ...) is an ordinary
  `IDENT`, resolved against a schema table at parse time — not a
  lexer-level keyword. `with`, `as`, `external`, and `use` are *contextual*
  keywords, meaningful only in the grammar position expected (the same
  technique as C#'s `var`/`async`/`await`/`yield`), not globally
  off-limits as identifiers.
- `.` separates an import alias from the name it qualifies (`alias.name`,
  see Imports, below) and never appears anywhere else in the grammar —
  `NUMBER` is integer-only, so there's no decimal-point ambiguity to
  resolve.
- `NUMBER` is integer-only: `[0-9]+`, no sign, no decimal point, no exponent.
- `STRING` is double-quoted with no escape sequences, and cannot contain a
  literal `"` or a newline — an unterminated string is a lex error.
- `->` is always a single token; a bare `-` is never valid on its own (it
  only ever appears inside an `IDENT`'s tail, or as the lead character of
  `->`).
- `{{name}}` (string interpolation, an implicit binding to the enclosing
  service's own name) is *not* part of the lexical grammar — it's ordinary
  content inside a `STRING` token, resolved later at codegen time.
- `#` line comments are skipped like whitespace; no comment token is
  emitted. A `#` inside a `STRING` is just ordinary string content, not a
  comment — comments are only recognized between tokens.

## Syntactic grammar

```
program        ::= top_decl*

top_decl       ::= named_decl | template_decl | use_decl

named_decl     ::= IDENT IDENT body

template_decl  ::= "template" IDENT param_list? ( body | "=" statement )

use_decl       ::= "use" STRING "as" IDENT

param_list     ::= "(" ( IDENT ( "," IDENT )* )? ")"

body           ::= "{" statement* "}"

statement      ::= key ":" value
                  | key sugar?

key            ::= IDENT | STRING

sugar          ::= body
                  | value ( "->" | "=" ) value
                  | value ( "," value )* statement*

value          ::= literal | list | statement

list           ::= "[" ( value ( "," value )* )? "]"

literal        ::= STRING | NUMBER | IDENT
```

`statement` is the whole language: a `named_decl` is one particular shape of
it (mandatory second name, mandatory body); every field inside a `service`,
every `template` invocation, every leaf like `image "foo"` is the same
`statement` production, applied recursively.

The grammar above is deliberately silent on layout, but layout isn't actually
free — two rules govern how statements are separated, neither expressible in
a plain context-free grammar (both depend on source position/line, not just
token identity):

- **Different fields in a struct-kind body are separated by a newline, never
  a comma.** `service`/`template`/`network`'s own top-level body, and a
  nested struct-kind type's canonical `{ }` form (`image { ... }`, `expose {
  ... }`, `restart { ... }`), all require this: `image "x"` and `restart
  unless-stopped` must be on separate lines, and a comma between them (`image
  "x", restart unless-stopped`) is a compile error, not a tolerated
  no-op — a comma is reserved exclusively for continuing a *single* field's
  own comma-list (see below), never for marking the boundary between two
  unrelated fields. A single-statement body needs nothing to separate
  (`{ image "x" }` on one line is fine); the rule only applies from the
  second statement on.
- **A comma-list's trailing comma is mandatory, not optional, to continue
  it.** A bracket list (`[a, b, c]`), a bare `with`-list (`with a, b, c`),
  and a primary-shorthand's own secondary fields (rule 3, below) all follow
  "trailing comma continues, its absence ends the statement" — but the
  comma itself is never optional when there *is* a next item; bare
  adjacency with no comma at all no longer implies continuation.

Map-kind bodies (`raw { }`, and a `with`-invocation's own argument body,
which reuses `raw`'s entry parsing) are exempt from the newline rule — their
entries are conceptually key-value pairs in a dictionary, not named struct
fields, and the compact one-line style (`{ puid: 1000, pgid: 100 }`) used
throughout this doc's own worked examples stays valid, comma-separated, on
one line.

### Desugaring rules

1. **Primary-value/list shorthand** — a type's schema may designate one
   field as primary. A bare value (or comma-list, if the primary field is
   list-typed) right after the type name, with no `{ }`, sets just that
   field. `image "foo"` desugars to `image { ref: "foo" }`.
2. **Map bare-entry shorthand** — a bare `<key> <sep> <value>` line
   desugars to a one-entry map, where `<sep>` is a per-type schema choice
   (`env` uses `=`, `volume` uses `->`). Both desugar to the same canonical
   `:`-separated map form internally.
3. **Secondary-field bare shorthand** — after a primary value, a type's
   schema-configured `bare_keyword_alias` (if it has one — `as` is the one
   built-in case, aliasing to `expose`'s `host` field) may fuse onto it
   directly with **no comma**: `expose port as "host"`. This is a one-shot
   continuation, not a list — it cannot itself be followed by anything
   else, comma or no comma; `expose port as "host", entrypoint: "..."` is a
   compile error. Beyond that, additional explicit `key: value`/`key`
   fields may follow, each preceded by a **mandatory comma** (the same
   "trailing comma continues, its absence ends the statement" rule as any
   other comma-list, including exempting the alias keyword itself — `as`
   isn't a valid target of this comma-continuation, only of the immediate
   no-comma fusion above): `expose port, host: "...", entrypoint: "..."`.
   A boolean struct field can always be set bare with no value, implying
   `true` (e.g. `external` on `network`). Field-init shorthand (`{ port }`
   standing in for `{ port: port }`, borrowed from Rust) and a bare
   zero-field template invocation (`authenticated` with no `{ }`) are the
   same grammar production as the comma-continuation case, disambiguated
   only by schema lookup — one token of lookahead past the comma confirms
   the next key genuinely names one of the nested type's own fields before
   consuming it as part of this value; otherwise the comma (and whatever
   follows) is left for the *enclosing* body, where a bare comma is never a
   valid statement start and now correctly errors instead of silently
   reattaching elsewhere.
4. **Repeatable-field accumulation** (semantic, not part of the CFG) —
   writing `volume`, `env`, `middleware`, or `depends_on` more than once in
   one body appends, since those fields are list/map-kinded. Writing
   `image` or `restart` twice in the same body is a duplicate-scalar
   compile error.

### Built-in schema table

| Type | Kind | Primary field | Separator | Uniqueness side | Needs name |
|---|---|---|---|---|---|
| `network` | struct | — | — | — | yes |
| `service` | struct | — | — | — | yes |
| `image` | struct | `ref` | — | — | no |
| `expose` | struct | `port` | — | — | no |
| `volume` | map | — | `->` | value (container path) | no |
| `env` | map | — | `=` | key | no |
| `restart` | struct | `policy` | — | — | no |
| `with` | struct | `templates` (list of nested instantiations) | — | — | no |
| `raw` | map | — | `:` | none (schema-free, passthrough) | no |

`middleware`, `depends_on`, `networks`, and `dns` are not rows in this
table — they're plain list-of-reference fields directly on
`service`/`template` (`dns ["192.168.50.182"]`: a per-service DNS
resolver override, Compose's own `dns:` key — the field itself is
generic, only a given entry's IP is homelab-specific, same reasoning as
`volume`'s host path or an `env` entry's value already being
homelab-specific without the field itself being one). `template` isn't a
row either — it's the mechanism for adding new rows to this table at
parse time. `defaults` is likewise not a row — it's an ordinary
template, semantically special only in that it's implicitly applied (see
Composition, below).

## Composition: templates and `with`

A `template` is a named, optionally parameterized block that produces a
*partial* record of fields, meant to be merged onto a real `service` via
`with`. Templates must be fully applied at each call — no partial
application, no currying. A template's body can itself `with` other
templates (composition).

`defaults` is not a reserved word — it's an ordinary template name the
compiler treats specially: if declared, it's implicitly applied at the
lowest-priority tier, below any explicit `with`-listed template, and never
participates in conflict-checking (it always silently loses).

Merge priority, lowest to highest:

1. the implicit `defaults` template, if declared
2. explicit `with`-listed templates, left to right — a collision between
   two of these on the same scalar/map field is a **compile error**
3. the service's own body — always wins over everything

List fields concatenate (no collision possible); map fields merge
key-by-key (or value-by-value for `volume`); scalar fields (`image`,
`restart`) error on collision among explicit templates only. `expose`,
the one built-in struct field with more than one sub-field, merges
per sub-field (`port`/`host`/`entrypoint` independently) rather than as
one indivisible unit — the same key-by-key reasoning as a map field,
applied to a struct's named fields instead of a map's keys. This means a
service's own body can override just `expose.host` while still
inheriting `port`/`entrypoint` from a `with`-listed template, without
repeating them; two explicit templates only collide if they set the
*same* `expose` sub-field, not merely the same `expose` field overall.

```
template internal_web(port) {
  expose port, entrypoint: "web-secure"
}

service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just expose.host — port and entrypoint still come from
  # internal_web above
  expose { host: "tools.internal.techdebtor.io" }
}
```

## Imports

Real-world templates and network declarations are meant to be shared
across many service files, not copy-pasted into each one. `use` imports
another `.hll` file under a local alias; `alias.name` then references
anything that file declares at its top level.

```
use "docker.hll" as traefik
```

- `use`'s path is always a quoted `STRING` — `IDENT`'s grammar
  (`[A-Za-z_][A-Za-z0-9_-]*`) can't represent `.`/`/` at all, so a bare
  path isn't lexable. It's resolved relative to the *importing file's
  own location*, never the entry file's location or the working
  directory the compiler was invoked from.
- `alias.name` qualifies any reference that would otherwise be a bare
  `IDENT`: a `networks [...]` entry (`networks [traefik.traefik-net]`)
  or a `with` invocation's target (`with common.internal_web { ... }`).
  `middleware`/`depends_on` don't support a qualified form — neither has
  a coherent cross-file meaning (`depends_on` names a same-file sibling
  service; `middleware` isn't resolved against anything at all).
- **Templates are lexically scoped, not dynamically scoped.** If a
  template declared in `templates.hll` writes
  `networks [traefik.traefik-net]`, that `traefik` resolves against
  *`templates.hll`'s own* `use` declarations — never whichever file
  happens to invoke the template with `with`. A template's references
  always resolve relative to where it was *written*, not where it was
  *called from*.
- **Imports are not transitive.** `use`-ing a file only makes *that
  file's* own top-level declarations available under your alias — not
  anything *it* in turn `use`s. If `service.hll` uses `templates.hll`,
  and `templates.hll` uses `docker.hll`, `service.hll` cannot write
  `docker.hll`'s alias itself; only `templates.hll`'s own template
  bodies can (via the lexical-scoping rule above).

## Worked examples

A plain service, no templates:

```
service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.techdebtor.io"
  volume "/mnt/media" -> "/data"
  env PUID = "1000"
  restart unless-stopped
}
```

Templates composed onto a service with `with`:

```
network traefik-net {
  external
  name: "docker_default"
}

template internal_web(port) {
  networks [traefik-net]
  restart unless-stopped
  expose port, host: "{{name}}.internal.techdebtor.io", entrypoint: "web-secure"
  middleware local-ipwhitelist
}

template authenticated {
  middleware forwardAuth-authentik
}

template linuxserver_app(puid, pgid) {
  env PUID = puid
  env PGID = pgid
}

service syncthing {
  with internal_web { port: 8384 }, authenticated, linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

The exact same templates, split across files via `use` instead of
copy-pasted into every service that needs them (this is
`crates/hl-cli/tests/fixtures/imports/` verbatim):

```
# network.hll
network traefik-net {
  external
  name: "docker_default"
}

# templates.hll
use "network.hll" as net

template internal_web(port) {
  networks [net.traefik-net]
  restart unless-stopped
  expose port, host: "{{name}}.internal.techdebtor.io", entrypoint: "web-secure"
  middleware local-ipwhitelist
}

template authenticated {
  middleware forwardAuth-authentik
}

template linuxserver_app(puid, pgid) {
  env PUID = puid
  env PGID = pgid
}

# syncthing.hll
use "templates.hll" as common

service syncthing {
  with common.internal_web { port: 8384 }, common.authenticated, common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

`syncthing.hll` never itself `use`s `network.hll` — only `templates.hll`
does — yet `internal_web`'s own `networks [net.traefik-net]` still
resolves correctly no matter which service ends up invoking it, since it
always resolves against `templates.hll`'s own alias table, never the
caller's.

A `with` list composing several templates reads as one long line once it
grows past two or three — per the Syntactic grammar section above, "a
trailing comma continues a comma-list," so the same `with` line can be
wrapped across multiple lines instead, one template per line, as long as
every line but the last ends with a trailing comma:

```
service syncthing {
  with common.internal_web { port: 8384 },
       common.authenticated,
       common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

This parses identically to the single-line form above — it's purely a
readability choice, not a different construct.

## Pipeline

1. **Lexer** (`crates/hl-lexer`) — one reserved word (`template`),
   string/number literals, `{`/`}`/`[`/`]`/`.`, `->`, `:`, `=`, `(`/`)`,
   `,`, and `#` line comments. Everything else is just an identifier to
   the lexer; meaning comes from the schema table during parsing.
2. **Parser** (`crates/hl-parser`) — one generic block parser, not one
   function per keyword: parse `<type> [<n>]`, then either a
   bare-value/list (primary-field shorthand) or a `{ field: value, ... }`
   body, recursing into nested blocks. A schema table drives both parsing
   and validation. Covers every built-in type (`network`, `service`,
   `image`, `expose`, `volume`, `env`, `restart`, `raw`), full
   `template`/`with` composition, and `use`/alias-qualified references
   (see Composition and Imports, above) — purely syntactic, no name
   resolution.
3. **Compose** (`crates/hl-parser`'s `compose` module) — resolves every
   `with`-list into a fully-merged `Service` with no templates or
   unresolved parameters left, per the Composition section's 3-tier merge
   rules. Generalized over a `SymbolResolver` trait so the same merge
   engine resolves both a single file's own templates (`compose`, no
   imports) and a whole `use` graph (`compose_with_resolver`, driven by
   the linker below).
4. **Linker** (`crates/hl-linker`) — loads a `use` graph off disk (or, for
   tests, an in-memory map) into a module graph, and implements
   `SymbolResolver` over it so `compose_with_resolver` can resolve
   cross-file `alias.name` references — see Imports, above.
5. **Codegen** (`crates/hl-codegen`) — walks a composed program and emits
   one Compose YAML document per input file (which may hold multiple
   services), with Traefik labels on each service's own `labels:` list.
6. **CLI** (`crates/hl-cli`, binary name `hllc`) — `hllc <file.hll>` lexes
   and prints tokens; `hllc --parse <file.hll>` parses and pretty-prints
   the AST; `hllc --build <file.hll> [--out <path>]` runs the full
   pipeline (link → compose → codegen) and writes (or, with no `--out`,
   prints) the resulting Compose YAML. `--build` also accepts a
   directory: every `.hll` file directly inside it (non-recursive) is
   built as its own independent entry point with its own `use` graph,
   each writing to `<out>/<stem>/docker-compose.yml`.

## Future work

- **`bootstrap` scaffold** — generate a brand-new homelab's starting
  `.hll` files from a template: a `docker.hll` declaring the shared
  network plus a `traefik` service (HTTPS termination, a
  certificate-resolver placeholder, the `web-secure`/`web` entrypoints),
  and a `templates.hll` with common reusable templates in the same shape
  as this doc's own worked examples — so starting a new homelab doesn't
  mean hand-writing the reverse-proxy service from scratch. Not yet
  designed: exactly where the line falls between what's generic enough to
  belong in the scaffold (entrypoints, the shape of a certificate
  resolver) versus what's homelab-specific and should stay a
  fill-in-the-blanks placeholder (DNS provider/credentials, domain, IP
  ranges) — see "Design principle: generic core, specific templates,"
  above.
- **`hllfmt`** — an auto-formatter that would wrap a long `with` list past
  some line length (see the multiline `with` example above) with
  consistent indentation, instead of that being a manual per-file
  judgment call. Not yet designed: the line-length threshold, and whether
  formatting is opinionated/non-configurable (à la `gofmt`/`rustfmt`) or
  takes any settings at all.
