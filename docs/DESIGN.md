# hl-lang design

`hl-lang` is a small declarative DSL that transpiles to Docker Compose YAML
plus Traefik labels. It is a transpiler, not an interpreter — no evaluation,
no closures, no runtime. This document is the language's spec: grammar,
semantics, and worked examples. It's the source of truth the lexer, parser,
and codegen implementations are built against.

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
Punctuation: { } [ ] ( ) : = -> ,
```

- `template` is the *only* reserved word in the entire language. Everything
  else that looks like a keyword (`service`, `network`, `image`, `volume`,
  `env`, `restart`, `expose`, `middleware`, `depends_on`, `networks`, `with`,
  `as`, `external`, `raw`, `defaults`, ...) is an ordinary `IDENT`, resolved
  against a schema table at parse time — not a lexer-level keyword. `with`,
  `as`, and `external` are *contextual* keywords, meaningful only in the
  grammar position expected (the same technique as C#'s `var`/`async`/
  `await`/`yield`), not globally off-limits as identifiers.
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

top_decl       ::= named_decl | template_decl

named_decl     ::= IDENT IDENT body

template_decl  ::= "template" IDENT param_list? ( body | "=" statement )

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
`statement` production, applied recursively. Statements need no separator
token — a trailing comma continues a comma-list, its absence unambiguously
ends the current statement, so newline-separated bodies parse correctly with
a single token of lookahead.

### Desugaring rules

1. **Primary-value/list shorthand** — a type's schema may designate one
   field as primary. A bare value (or comma-list, if the primary field is
   list-typed) right after the type name, with no `{ }`, sets just that
   field. `image "foo"` desugars to `image { ref: "foo" }`.
2. **Map bare-entry shorthand** — a bare `<key> <sep> <value>` line
   desugars to a one-entry map, where `<sep>` is a per-type schema choice
   (`env` uses `=`, `volume` uses `->`). Both desugar to the same canonical
   `:`-separated map form internally.
3. **Secondary-field bare shorthand** — after a primary value, additional
   bare `key: value`/`key` statements set other non-primary struct fields
   of the same type. A boolean struct field can always be set bare with no
   value, implying `true` (e.g. `external` on `network`). Field-init
   shorthand (`{ port }` standing in for `{ port: port }`, borrowed from
   Rust) and a bare zero-field template invocation (`authenticated` with no
   `{ }`) are the same grammar production as this rule, disambiguated only
   by schema lookup.
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

`middleware`, `depends_on`, and `networks` are not rows in this table —
they're plain list-of-reference fields directly on `service`/`template`.
`template` isn't a row either — it's the mechanism for adding new rows to
this table at parse time. `defaults` is likewise not a row — it's an
ordinary template, semantically special only in that it's implicitly
applied (see Composition, below).

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
key-by-key (or value-by-value for `volume`); struct/scalar fields error on
collision among explicit templates only.

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
template internal_web(port) {
  networks [traefik-net]
  restart unless-stopped
  expose port as "{{name}}.internal.techdebtor.io" entrypoint: "web-secure"
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

## Pipeline

1. **Lexer** (implemented, `crates/hl-lexer`) — one reserved word
   (`template`), string/number literals, `{`/`}`/`[`/`]`, `->`, `:`, `=`,
   `(`/`)`, `,`, and `#` line comments. Everything else is just an
   identifier to the lexer; meaning comes from the schema table during
   parsing.
2. **Parser** (implemented, `crates/hl-parser`) — one generic block parser,
   not one function per keyword: parse `<type> [<n>]`, then either a
   bare-value/list (primary-field shorthand) or a `{ field: value, ... }`
   body, recursing into nested blocks. A schema table drives both parsing
   and validation. **Scope note:** this milestone covers only the built-in
   types (`network`, `service`, `image`, `expose`, `volume`, `env`,
   `restart`, `raw`); `template` declarations and the `with` field are
   rejected with a clear "not yet supported" parse error rather than
   parsed — template/`with` composition is a fast-follow milestone.
3. **AST → codegen** (not yet implemented) — walk the AST, emit two
   artifacts per service: a Compose service block, and Traefik labels
   (router rule, entrypoint, TLS resolver).
4. **CLI** (`crates/hl-cli`) — `hl-cli <file.hll>` lexes and prints tokens;
   `hl-cli --parse <file.hll>` parses and pretty-prints the AST. Will grow
   into `hl-cli build up.hll` / `hl-cli build ./services/` once codegen
   exists.
