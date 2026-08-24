# hll design

`hll` (pronounced "hell"—short for **H**ome**L**ab **L**anguage) is a
small declarative Domain-Specific Language (DSL) that transpiles to Docker
Compose YAML plus Traefik labels. It's a transpiler, not an
interpreter—no evaluation, no closures, no runtime. This document is the
language's spec: grammar, semantics, and worked examples. It's the source
of truth that the lexer, parser, and codegen implementations build
against. Source files use the `.hll` extension, and `hllc` is the
command-line tool.

## Motivation

Standing up a new homelab service usually means rewriting a near-identical
Docker Compose service block plus Traefik labels: image, port, subdomain,
volume, restart policy, sometimes Authentik forward-auth. `hl-lang` removes
that repetition by compiling a compact declaration down to the Compose YAML
and Traefik labels that would otherwise be hand-written.

## Design principle: generic core, specific templates

The compiler's built-in schema stays small and generically
Docker-Compose/Traefik-shaped—it has no knowledge of any particular
homelab's conventions, such as specific auth providers, domain names, or
Process User Identifier (PUID) and Process Group Identifier (PGID)
values. Anything that's actually about *one* homelab belongs in
`template` files that get imported, not in the
compiler itself. The test to apply when considering a new built-in:
"would this make sense on a homelab with completely different
infrastructure?" If not, it's a template, not a grammar feature.

## Lexical grammar

```
IDENT   ::= [A-Za-z_][A-Za-z0-9_-]*
NUMBER  ::= [0-9]+
STRING  ::= '"' ( [^"\\\n] | ESCAPE )* '"'
ESCAPE  ::= '\\' ( '"' | '\\' | 'n' | 't' | 'r' )
COMMENT ::= '#' [^\n]*        # to end of line; not part of the token stream

Reserved (not usable as IDENT): "template"
Punctuation: { } [ ] ( ) : = -> , . $
```

- `template` is the *only* reserved word in the entire language. Everything
  else that looks like a keyword (`service`, `network`, `image`, `volume`,
  `publish`, `env`, `env_file`, `restart`, `expose`, `healthcheck`,
  `middleware`, `depends_on`, `networks`, `dns`, `devices`,
  `container_name`, `command`, `entrypoint`, `privileged`, `with`, `as`,
  `external`, `use`, `raw`, `defaults`, and more) is an ordinary
  `IDENT`, resolved against a schema table at parse time—not a
  lexer-level keyword. `with`, `as`, `external`, and `use` are
  *contextual* keywords, meaningful only in the grammar position expected
  (the same technique as C#'s `var`/`async`/`await`/`yield`), not
  globally off-limits as identifiers.
- `.` separates an import alias from the name it qualifies (`alias.name`,
  see Imports, below) and never appears anywhere else in the grammar—
  `NUMBER` is integer-only, so there's no decimal-point ambiguity to
  resolve.
- `$` prefixes a reference to a `template`'s own declared parameter
  (`$port`), see Composition, below. It's reserved for exactly that one
  purpose—not a general sigil for anything else.
- `NUMBER` is integer-only: `[0-9]+`, no sign, no decimal point, no exponent.
- `STRING` is double-quoted, and a backslash escapes the character after
  it. The five escape sequences are `\"`, `\\`, `\n`, `\t`, and `\r`, and
  a backslash followed by anything else is a lex error—an escape the
  language doesn't have never means the two characters that spell it.
- A `STRING` still can't span a literal newline in the source: write the
  newline as `\n` instead. An unterminated string—one whose line ends
  before its closing `"`—is a lex error, and so is a string ending in a
  backslash, since that backslash escapes the closing quote and leaves
  the literal open.
- `->` is always a single token. A bare `-` is never valid on its own (it
  only ever appears inside an `IDENT`'s tail, or as the lead character of
  `->`).
- `{{name}}` (string interpolation, an implicit binding to the enclosing
  service's own name) is *not* part of the lexical grammar—it's ordinary
  content inside a `STRING` token, resolved later at codegen time.
- The lexer skips `#` line comments like whitespace and emits no comment
  token. A `#` inside a `STRING` is just ordinary string content, not a
  comment—comments are only recognized between tokens.

## Syntactic grammar

```
program        ::= top_decl*

top_decl       ::= named_decl | template_decl | use_decl

named_decl     ::= IDENT IDENT body

template_decl  ::= "template" IDENT param_list? body

use_decl       ::= "use" STRING "as" IDENT

param_list     ::= "(" ( param ( "," param )* )? ")"

param          ::= IDENT ( ":" param_type )?

param_type     ::= "Number" | "String"

body           ::= "{" statement* "}"

statement      ::= key ":" value
                  | key sugar?

key            ::= IDENT | STRING

sugar          ::= body
                  | value ( "->" | "=" ) value
                  | value ( "," value )* statement*
                  | IDENT? body
                  | IDENT ( "," statement )*

value          ::= literal | list | statement

list           ::= "[" ( value ( "," value )* )? "]"

literal        ::= STRING | NUMBER | IDENT | "$" IDENT
```

- The last two `sugar` alternatives belong to a **name-keyed** field, of
  which `router` is the only one—see the schema table below. The
  identifier between the field name and the body names *this* instance
  of the field rather than setting a sub-field, so `router api { host:
  "..." }` and `router api, host: "..."` both declare a router called
  `api`. The name is an `IDENT`, never a `STRING`, matching the spelling
  of a top-level declaration's own name: it ends up in a Traefik
  label *key*, and `IDENT`'s grammar can't hold the `.`, `=`, backtick,
  or space that would forge a different key. Leaving the name off
  requires the braced form, since a comma-list needs a first token to
  continue from and `router host: "x"` can't say whether `host` names the
  router or its own field.
- The parser checks a parameter's optional `: param_type` annotation
  strictly, not coercively: a declared `Number` rejects a quoted string
  argument even if it's numeric-looking, and a declared `String` rejects
  a bare number—no implicit coercion between kinds. An untyped parameter,
  with no `:` at all, accepts an argument of any literal kind, with no
  Compose-time check. `Number`/`String` are the only two types this
  milestone supports—a bare-`IDENT`-typed or list-typed parameter isn't
  expressible yet.
- The `"$" IDENT` form of `literal`, a parameter reference such as
  `$port`, is only legal inside a `template`'s own body—including a
  nested `with`-invocation argument body written inside that template,
  where a `$name` forwards the *enclosing* template's own parameter (see
  Composition, below). Used anywhere else (a plain `service`/`network`
  body, or a `with`-invocation body written inside one of those), it's a
  compile error: only a template body has a declared parameter list to
  resolve `$name` against. This is, like the preceding newline/comma
  layout rules, a context-sensitive constraint the plain grammar can't
  express.

`statement` is the whole language: a `named_decl` is one particular shape
of it with a mandatory second name and mandatory body. Every field
inside a `service`, every `template` invocation, every leaf like `image
"foo"` is the same `statement` production, applied recursively.

The preceding grammar is deliberately silent on layout, but layout isn't
actually free—two rules govern how a newline or a comma separates
statements, neither expressible in a plain context-free grammar, since
both depend on source position/line, not just token identity:

- **A struct-kind body separates different fields with a newline, never
  a comma.** `service`/`template`/`network`'s own top-level body, and a
  nested struct-kind type's canonical `{ }` form (`image { ... }`, `expose {
  ... }`, `restart { ... }`), all require this: `image "x"` and `restart
  unless-stopped` must be on separate lines, and a comma between them (`image
  "x", restart unless-stopped`) is a compile error, not a tolerated
  no-op—a comma exclusively continues a *single* field's own comma-list,
  described in the next bullet below, and never marks the boundary
  between two unrelated fields.
  A single-statement body needs nothing to separate—`{ image "x" }` on
  one line is fine. The rule only applies from the second statement on.
- **A comma-list's trailing comma is mandatory, not optional, to continue
  it.** A bracket list (`[a, b, c]`), a bare `with`-list (`with a, b, c`),
  and a primary-shorthand's own secondary fields, per rule 3 below, all
  follow "trailing comma continues, its absence ends the statement"—but
  the comma itself is never optional when there *is* a next item. Bare
  adjacency with no comma at all no longer implies continuation.

Map-kind bodies—`raw { }`, `volume { }`/`publish { }`/`env { }`, and a
`with`-invocation's own argument body (which reuses `raw`'s entry
parsing)—are exempt from the newline rule in the *opposite* direction
from the preceding comma-list rule: their entries are conceptually
key-value pairs in a dictionary, not named struct fields, and either a
comma *or* a newline, not just a comma, is enough to separate them, so
the compact one-line style (`{ puid: 1000, pgid: 100 }`) used throughout
this doc's own worked examples stays valid, comma-separated, on one line,
alongside the equally valid multi-line form with no commas at all. What's
never valid is bare adjacency on *one* line with neither:
`{ "a": "/x" "b": "/y" }` is a parse error (`expected a comma
or a newline before the next entry`), same as the comma-list rule's own
"bare adjacency no longer implies continuation"—only here a newline is
also an accepted substitute for the comma, not just its own separate case.
Both `{ "a": "/x", "b": "/y" }` and `{ "a": "/x"\n  "b": "/y" }` parse to the
same two entries—see #81.

### Desugaring rules

1. **Primary-value/list shorthand**—a type's schema may designate one
   field as primary. A bare value (or comma-list, if the primary field is
   list-typed) right after the type name, with no `{ }`, sets just that
   field. `image "foo"` desugars to `image { ref: "foo" }`.
2. **Map bare-entry shorthand**—a bare `<key> <sep> <value>` line
   desugars to a one-entry map, where `<sep>` is a per-type schema choice
   (`env` uses `=`, `volume` uses `->`). Both desugar to the same canonical
   `:`-separated map form internally.
3. **Secondary-field bare shorthand**—after a primary value, a type's
   schema-configured `bare_keyword_alias` (if it has one—`as` is the one
   built-in case, aliasing to `expose`'s `host` field) may fuse onto it
   directly with **no comma**: `expose port as "host"`. This is a one-shot
   continuation, not a list—nothing else can follow it with or without
   a comma. `expose port as "host", entrypoint: web` is a compile error.
   Beyond that, additional explicit `key: value`/`key` fields may follow,
   each preceded by a **mandatory comma** (the same "trailing comma
   continues, its absence ends the statement" rule as any other
   comma-list, including exempting the alias keyword itself—`as` isn't
   a valid target of this comma-continuation, only of the preceding
   no-comma fusion): `expose port, host: "...", entrypoint: web`.
   A field whose own value is an unbracketed comma-list (`entrypoint`'s
   reference list) ends at the next `key:` rather than swallowing it, by
   the same one-token lookahead: in `expose port, entrypoint: web, host:
   "..."`, the second comma starts a sibling field of `expose`, not a
   second entry point.
   Writing a boolean struct field bare, with no value, always implies
   `true` (for example, `external` on `network`). A bare zero-field
   template invocation (`authenticated` with no `{ }`) is the same
   grammar production as the comma-continuation case, disambiguated only
   by schema lookup—one token of lookahead past the comma confirms the
   next key genuinely names one of the nested type's own fields before
   consuming it as part of this value. Otherwise the comma and whatever
   follows it stay with the *enclosing* body, where a bare comma is never
   a valid statement start and now correctly errors instead of silently
   reattaching elsewhere.
4. **Repeatable-field accumulation**—semantic, not part of the
   Context-Free Grammar (CFG)—writing `volume`, `publish`, `env`,
   `middleware`, `depends_on`, or `router` more than once in
   one body appends, since those fields are list/map-kinded—subject to
   the set-like lists' distinct-name rule under "Composition" below, which
   drops a repeat of a name already present (`depends_on` instead keeps
   only its own list's *last* entry for a repeated name, per the same
   keyed-merge rule its own paragraph under "Composition" describes—#155).
   `router` appends only for *distinct* names: two blocks in one body
   claiming the same router id is a compile error rather than an
   append or an override, since that isn't two routers but one router
   described twice—#184.
   Writing `image` or `restart` twice in the same body is a
   duplicate-scalar compile error.

### Built-in schema table

| Type | Kind | Primary field | Separator | Uniqueness side | Needs name |
|---|---|---|---|---|---|
| `network` | struct |—|—|—| yes |
| `volume`—the top-level declaration | struct |—|—|—| yes |
| `service` | struct |—|—|—| yes |
| `image` | struct | `ref` |—|—| no |
| `expose` | struct | `port` |—|—| no |
| `router` | struct |—|—| the router name | optional |
| `volume`—the `service`/`template` field | map |—| `->` | value—the container path | no |
| `driver_opts`—inside a `volume` declaration | map |—| `:` | key | no |
| `publish` | map |—| `->` | value—the container port | no |
| `devices` | map |—| `->` | value—the container device path | no |
| `env` | map |—| `=` | key | no |
| `restart` | struct | `policy` |—|—| no |
| `healthcheck` | struct |—|—|—| no |
| `traefik` | struct |—|—|—| no |
| `with` | struct | `templates`—list of nested instantiations |—|—| no |
| `raw` | map |—| `:` | none—schema-free, passthrough | no |

`volume` has two rows because the identifier plays two roles: at the top
level it *declares* a named Docker volume, as in `volume
syncthing-config { external, name: "...", driver: "...", driver_opts {
... } }`, and inside a `service`/`template` body it *mounts* one. The
grammar stays unambiguous—the parser resolves a top-level type name only
through `schema::top_level_type` and a field name only through
`schema::resolve_field` against the enclosing type's own field list, and
consults neither table in the other's position. `network` has no
equivalent pair today only because no field goes by the literal name
`network`, since a service's list field is `networks`.

`volume`'s service-level field, `publish`, and `devices` are three
schema rows over one shared shape, unified at #192: a `->`-separated
`host -> container` arrow map with uniqueness on the container side. All
three parse and merge through one shared `ArrowMap`/`ArrowMapEntry` tree
type rather than three near-identical ones, since the only things that
ever varied row to row were two schema-driven bits, both covered below:
whether the host side may name a declared top-level `volume`—
`key_may_be_reference`, true for `volume` alone—and whether an entry
carries a trailing `{ read_only }` modifier, likewise `volume`-only.
`env`, `driver_opts`, and `raw` are map-kind too but outside this
group—their uniqueness lands on the key side instead, per the preceding
table, so they never shared `volume`/`publish`/`devices`' own value-side
convention to begin with.

The `volume` field is also the one map-kind type whose *key* side isn't
restricted to a literal. Its host side is either a string, meaning a
bind-mount path, or an identifier, optionally `alias.`-qualified,
meaning a reference to a top-level `volume`
declaration—`TypeSchema::key_may_be_reference`, true for that one type,
is what selects the entry parser that draws the distinction. Every other
map key—`env`, `publish`, `devices`, `driver_opts`, `raw`—stays a plain
literal, since none of them names anything an `.hll` file declares.

Every `volume` entry, on either side of that host-kind split, may also
carry an optional trailing `{ read_only }` body—`volume "/" ->
"/rootfs" { read_only }`—appending Compose short syntax's `:ro` mode
suffix to the emitted mount string, per #158. `depends_on`'s own
per-entry `{ condition: ... }` body, covered later in this section,
uses the identical shape: an entry that already parsed its primary pair
can add `{ }` next, and the parser only checks for that per-entry `{`
after parsing the pair, not at the point where the `SchemaKind::Map`
branch decides whether the whole field opens with a canonical
multi-entry `{ }` body or a single bare entry—so the two `{`s never
compete for the same position. `read_only` is bare presence, matching
`NETWORK`'s own `external`, not a `key: value` pair, since the only
value this milestone tracks is present or absent.

This design rejects two of the issue's own candidate shapes before
landing on this one. A trailing bare flag after the primary form—`volume
"/" -> "/rootfs", read_only`—turns out genuinely ambiguous, not just
unfamiliar: inside `volume`'s existing canonical multi-entry body, a
comma already separates one entry from the next, and a bare identifier
there already names the host side of a named-volume entry—so `volume {
"/" -> "/rootfs", read_only -> "/mnt2" }` already mounts a valid
two-entry list, one entry naming a volume literally called `read_only`.
Telling "a flag on the entry before the comma" apart from "the start of
a new entry that ran out of input before its own `->`" would need
unbounded lookahead the rest of the grammar never asks for. A `mode`
sub-field—`{ mode: "ro" }`—fails on scope alone, not ambiguity: this
milestone deliberately covers only `:ro`, leaving Compose's other
short-syntax suffixes (the `z`/`Z` SELinux relabeling flags, tmpfs
sizing) for a future issue, and a general string field would leave every
value but `"ro"` silently unchecked rather than rejected the way an
unknown bare flag already gets rejected.

`merge_map`'s existing full-entry replacement gives `read_only` the
correct override behavior for free—see "Composition" later in this
document. A later tier's entry with the same container path replaces
the earlier arrow-map entry outright, flag included, instead of merging
field by field, so an overriding entry that writes no flag drops an
inherited one, and an overriding entry that writes the flag never loses
it. This field needed no merge-path change.

`raw`'s "no uniqueness checking" is a *parser*-level statement: the
parser never checks its own entries against each other or against any
other field. Codegen does have one rule about them, because YAML forces
the issue—two keys spelled the same in one mapping is invalid—and a
`raw` key may well name a field that has a preceding row. Where it does,
codegen emits the `raw` value and drops the built-in one, which keeps
adding a row to this table from breaking files that were already
reaching for `raw` in that row's absence (see the `raw` section of the
book's Built-in Fields page).

`raw`'s own job has narrowed as the preceding schema table has grown.
Early on, before this table had more than a handful of rows, `raw` stood
in for nearly everything—it was, practically, the only way to reach most
of Compose's service-level keys. Each row this table gains since then is
one fewer key that has to route through it, so `raw`'s job today is
better described as the genuine long tail: real Compose keys that come
up too rarely, or are too specific to one deployment, to earn a row of
their own. `privileged`/`devices` are the most recent pair to graduate
out of it—see #157—following `dns`/`env_file`/`healthcheck` before them,
leaving keys like `security_opt` as the kind of entry that stays in
`raw` for good.

`publish` and `expose` are separate rows on purpose, not two spellings
of one concept. `publish` is Compose's `ports:` key, which puts the port
on the Docker host where the local network can reach it, and `expose` is
Compose's `expose:` key, which reaches only other containers on the same
network, plus the Traefik router labels. A homelab needs both: much of
it sits behind Traefik and wants `expose`, while Pi-hole on 53, a
Syncthing sync port, or a game server takes traffic directly and wants
`publish`—see #84. `publish`'s own uniqueness lands on the container
port rather than the host one because a protocol suffix rides on the
container half of a Compose short-syntax mapping (`53:53/udp`), so
checking the host side would reject one host port serving both
protocols—exactly the configuration the field exists to express.

`devices` shares `publish`'s reasoning for keying uniqueness on the
container side, right down to the optional suffix: Compose's own
`devices:` short syntax is `HOST:CONTAINER[:CGROUP_PERMISSIONS]`, so an
optional `rwm`-style control-group permissions suffix rides the
container half of a mapping—`"/dev/sda" -> "/dev/xvda:rwm"`—exactly the
way a protocol suffix rides `publish`'s own container half. It shipped
originally at #157 as a `FieldKind::ReferenceList` taking a single
pre-joined `"host:container"` string, `devices
["/dev/kmsg:/dev/kmsg"]`. #167 replaced that with this arrow-mapped
shape after review feedback pointed out the inconsistency with
`publish`/`volume`'s own spelling, merging through the same `merge_map`
those two fields use, keyed on the container side, rather than through
`LIST_FIELDS`'s set-like concatenation. #192 then folded all three
fields' own tree types together into the shared `ArrowMap`/
`ArrowMapEntry` pair described in the preceding paragraphs, since by
then all three were already identical `merge_map` calls differing only
in which field name and which `ServiceFields` slot each read.

`expose`'s own `entrypoint` sub-field is a list of references too
(`entrypoint web, web-secure`), for the same reason it isn't a scalar
anywhere else in the pipeline: Traefik's `entrypoints=` label is
comma-separated, and modelling that as a list keeps the separator
codegen's to write rather than the user's—so no generated label value
ever has to tolerate a user-written comma, and the metacharacter guard
can reject `,` uniformly everywhere.

`router` is the one row that's both repeatable *and* struct-kind, and
the only one whose instances take a key from a name the user writes. It
exists because `expose` models exactly one Traefik router—one `host`,
one `Host()` rule, one `entrypoints=` label—and a real service often
needs several off one container: a public host beside a local-network
host, an API path prefix split off from a catch-all frontend. Before #184 the only
way to say that was to abandon `expose` and hand-write the whole label
list in `raw { labels: [...] }`, which gives up `expose`'s validation,
its `{{name}}` interpolation, and its metacharacter guard for a service
that's otherwise an ordinary Traefik-fronted container.

`expose` is deliberately untouched by this. Making `expose` itself
repeatable would have broken its documented per-sub-field merge, and it
carries `port`, which is per Compose *service* rather than per router—a
second `expose` would have had to either duplicate the port or leave it
ambiguous. So `router` is a separate field, and a file that never writes
one compiles to exactly the bytes it compiled to before the field
existed.

Each block emits `traefik.http.routers.<service>-<name>` for its labels,
or `traefik.http.routers.<service>` for the unnamed `router { }` form—
the very id `expose.host` produces, which is why setting both is a
compile error rather than one silently overwriting the other. Two blocks
in one body claiming the same id is likewise an error, the same rule two
`volume` entries at one container path already follow: that isn't two
routers, it's one router described twice.

`host` is a plain scalar. `entrypoint` takes the same spelling and the
same merge rule as `expose.entrypoint`—two fields producing the same
`entrypoints=` label shouldn't be two different grammars. `path_prefix`
holds plain literals rather than references, and it's the one list field
that does:
a prefix is free text a template legitimately fills in with a `$param`,
and a reference has no `$param` form at all, since the grammar has no
parameter in reference position. With prefixes set, the rule
becomes ``Host(`h`) && (PathPrefix(`a`) || PathPrefix(`b`))``. The
parentheses are load-bearing, not cosmetic: `&&` binds tighter than `||`
in Traefik's rule grammar, so without them the rule would match *any*
host under the last prefix. They're emitted for a single prefix too,
where they change nothing, so the rule's shape doesn't depend on how many
prefixes it happens to have.

Two things a `router` deliberately doesn't carry. It has no `port`:
Compose's `loadbalancer.server.port` label is per Compose service, not
per router, so it stays derived from `expose.port` and several routers
off one container all balance onto that one port. And it has no
`middleware`: that stays a service-level field and applies to every
router the service has, so a service with three routers and one
`middleware` line gets the same middleware list on all three. Per-router
middleware isn't expressible yet.

Besides the three top-level types, `healthcheck`, `traefik`, and
`router` are the table's struct-kind rows with no primary field.
`router`'s reason is its own: the router's name already occupies the
position right after the keyword, so there is nowhere for a bare
primary value to go—write `router api { host: "..." }` or `router api,
host: "..."` instead. For the other two, unlike `image`'s `ref`
or `expose`'s `port`, no single sub-field of `healthcheck`'s
`test`/`interval`/`timeout`/`retries`/`start_period`/`start_interval`/
`disable`, nor of `traefik`'s own lone `disabled`, obviously stands in
for the whole struct, so both require the braced body—`healthcheck
"..."` and `traefik disabled` are parse errors rather than sugar for
anything. `test` needs a field kind none of the other struct fields do:
`FieldKind::ScalarOrList` accepts either a bare literal (Compose's shell
form, `CMD-SHELL <string>`) or a bracketed list of literals (Compose's
exec form, `["CMD", "pg_isready", "-U", "miniflux"]`), single-occurrence
like a plain scalar but with no bare comma-list sugar—`test: "a", "b"`
would be ambiguous between the shell string plus garbage and a two-item
exec list, so only a bare literal or an explicit `[...]` parses.
`disable` mirrors `NETWORK`'s `external` directly: a bare-presence
`FieldKind::BoolFlag`, matching Compose's own `disable: true`, which
turns the healthcheck off entirely, including one inherited from the
image. Every field here is a plain, generic Compose key, not
homelab-specific in any of its own fields—the same "generic core"
reasoning that already justified `dns`/`env_file`/`container_name`, see
#153.

`traefik`'s own `disabled` mirrors that same `disable`/`external`
bare-presence shape directly, but for the opposite reason: it isn't a
generic Compose key at all, it's the one label `hll`'s own Traefik
support computes but a service can now switch off—see #159. The issue
that motivated it floats a brace-free `traefik disabled` spelling first,
but that doesn't fit the schema engine: a brace-free form exists only
for a type with a `primary_field`, and `FieldKind::BoolFlag` can never
be one—a primary field always supplies a *value*
(`parse_struct_primary_shorthand`'s only bare-value path calls
`parse_literal`), while a bare-presence flag carries no value beyond
itself. Making `traefik { disabled }`'s braced form the only spelling
costs nothing beyond what `healthcheck { disable }` already pays for,
and leaves `traefik` a home a later Traefik knob can join without
inventing a second `traefik`-prefixed field name.

`middleware`, `depends_on`, `networks`, `dns`, and `env_file` aren't
rows in this table—they're plain list-of-reference fields directly on
`service`/`template` (`dns ["192.168.50.182"]`: a per-service Domain
Name System (DNS) resolver override, Compose's own `dns:` key—the field
itself is generic, only a given entry's IP is homelab-specific, same
reasoning as `volume`'s host path or an `env` entry's value already
being homelab-specific without the field itself being one). `env_file`
(`env_file "miniflux.env"` / `env_file ["miniflux.env", "common.env"]`)
follows the exact same reasoning as `dns`—Compose's own `env_file:` key,
generic itself even though a real entry almost always names a
gitignored, homelab-specific `.env` file—see #154. `devices` used to belong on this list too, as `devices
["/dev/kmsg:/dev/kmsg"]` at #157, but #167 gave it a `->`-mapped shape
instead—see the preceding `publish`/`devices` paragraph, where it's now
a row in the schema table rather than a plain reference list.

`depends_on` (`depends_on database` / `depends_on [database { condition:
service_healthy }]`) shares this row's surface grammar—a bare reference,
a bracketed list, the same accumulate-across-repeats rule—but each entry
may also carry an optional `{ condition: ... }` body naming one of
Compose's own three `depends_on` conditions: `service_started` (the
default, and the only thing a bare `depends_on database` has ever
meant), `service_healthy`, and `service_completed_successfully`—see
#155. A
`condition` value outside that fixed set of three is a compile error,
checked in the parser at the point it's written (mirroring
`UnknownParamType`'s own precedent for validating a literal's *value*,
not just its syntactic kind, as early as possible)—there's no later
stage this needs deferring to, since `condition` can't hold a `$param`
reference the way an ordinary literal slot can. `hllc` does *not* warn
when a `service_healthy` entry's target has no `.hll`-level
`healthcheck` field: a Docker image can bake its own `HEALTHCHECK` into
its Dockerfile, invisible to anything an `.hll` file declares, so a
missing `healthcheck` field isn't evidence the condition is
meaningless.

Compose's `depends_on:` key has two shapes that can't mix in one
document. The short form is a plain list of names and means "wait for
container start." The long form is a mapping of name to
`{ condition: ... }` and requires every entry to be a mapping. Codegen
emits the short form—unchanged from before this syntax existed—as long
as no entry in a service's `depends_on` carries a condition, and the
long form once any entry does, filling in `service_started` for any
sibling entry left bare.

`container_name` isn't a row either, for the opposite reason: it's a
plain *scalar* field directly on `service`/`template`
(`container_name "uptime-kuma"` / `container_name: "uptime-kuma"`)
rather than a nested struct type—it has no secondary fields of its own
to give it a primary-field/separator shape worth a table row. Unset,
it's simply omitted from the generated service block rather than
defaulting to anything—see #90. Compose's own per-project default
naming is what most people want, and defaulting the built-in to the
service's own name reliably collided across independent stacks sharing
a common service name. `command`, added in #156, isn't a row either,
for the same reason as `container_name`: a plain field directly on
`service`/`template` (`command "npm start"` / `command ["npm",
"start"]`) rather than a nested struct type. Its kind is
`FieldKind::ScalarOrList`, not `FieldKind::Scalar`, though—Compose's
`command:` key takes `healthcheck.test`'s own shell-string-or-exec-list
shape, overriding the image's entrypoint arguments rather than naming a
health check—so `command` follows `test`'s own model everywhere but its
position directly on `ServiceFields`, unlike `container_name`. Unset,
it's simply omitted, leaving the image's own `CMD`/entrypoint in effect,
the same "omit rather than default" rule `container_name` follows.
`entrypoint`, added in #183, isn't a row either, and is `command`'s
direct counterpart: the same `FieldKind::ScalarOrList` field directly on
`service`/`template` (`entrypoint "/bin/sh -c 'do-a-thing'"` /
`entrypoint ["/bin/sh", "-c", "do-a-thing"]`), because Compose gives its
`entrypoint:` key exactly the two forms it gives `command:`. The two are
separate Compose keys, not two spellings of one: `entrypoint` overrides
the image's `ENTRYPOINT` and `command` overrides its `CMD`, so a service
may set either, both, or neither, and setting one says nothing about the
other. Unset, `entrypoint` is simply omitted, leaving the image's own
`ENTRYPOINT` in effect.

The identifier `entrypoint` now names two unrelated things, the way
`volume` already does. `expose`'s own `entrypoint` sub-field is a
reference list of Traefik entry-point names, and the new one is a
service-level command override. The grammar stays unambiguous for
exactly the reason `volume`'s two roles do: the parser resolves a field
name only through `schema::resolve_field` against the enclosing type's
own field list, so `entrypoint` written in a `service`/`template` body
reaches `SERVICE_FIELDS`'s row and `entrypoint` written inside an
`expose { }` body—or after an `expose` shorthand's comma, where rule 3's
one-token lookahead consults `EXPOSE`'s field list and nothing else—
reaches `EXPOSE`'s. The parser consults neither table in the other's
position, and neither role involves a lexer-level keyword. The layout
rules keep the two apart at the statement level too: a struct-kind body
separates fields with a newline, so a service's own `entrypoint`
statement always begins a new statement rather than continuing the
preceding `expose` shorthand, which only ever continues past a comma.

`privileged` isn't a row either, for the same
reason `NETWORK`'s `external` isn't: a bare-presence `FieldKind::BoolFlag`
directly on `service`/`template`, matching Compose's own `privileged:`
key—see #157. `template` isn't a
row either—it's the mechanism for adding new rows to this table at
parse time. `defaults` is likewise not a row—it's an ordinary template,
semantically special only in that it's implicitly applied—see
Composition, below.

## Composition: templates and `with`

A `template` is a named, optionally parameterized block that produces a
*partial* record of fields for `with` to merge onto a real `service`.
Templates must be fully applied at each call: never partially applied,
and never curried. A template's body can itself `with` other
templates—composition.

`defaults` isn't a reserved word—it's an ordinary template name the
compiler treats specially: if declared, it's implicitly applied at the
lowest-priority tier, below any explicit `with`-listed template, and
never participates in conflict-checking—it always silently loses.

Merge priority, lowest to highest:

1. the implicit `defaults` template, if declared
2. explicit `with`-listed templates, left to right—a collision between
   two of these on the same scalar/map field is a **compile error**
3. the service's own body—always wins over everything

List fields concatenate, so no collision is possible. The set-like ones
(`middleware`, `networks`, `expose.entrypoint`) concatenate by
*distinct* name, keeping the first occurrence, while `dns` and
`env_file` keep duplicates since their order is observable—resolver
priority for `dns`, Compose's own last-file-wins precedence for
`env_file`—see #154. Map fields merge key-by-key, or value-by-value for
`volume`, `publish`, and `devices`, and scalar fields (`image`,
`restart`) error on collision among explicit templates only. `devices`
used to sit in the set-like group too at #157, giving a repeated
`"host:container"` mapping the same first-occurrence-wins treatment a
repeated `networks`/`middleware` entry got, since there was no
order-dependent Compose behavior under which naming one twice meant
anything different from naming it once. #167 moved it onto the same
key-by-key `merge_map` path `volume`/`publish` use instead, keyed on the
container side—see the preceding schema table's `publish`/`devices`
paragraph—which happens to produce the same "own wins, defaults loses,
two explicit collide" result for the common case of the same tier
repeating the same mapping, but now raises a genuine `MapKeyCollision`,
the same one `publish` would, when two *explicit* templates map
different hosts onto the same container path. `privileged` gets the
same collision rule as a scalar
field even though it isn't one—see the `healthcheck.test`/`.disable`
paragraph below for how a bare-presence flag rides the same
Own-always-wins/`defaults`-always-loses/two-explicit-collide rule
through `merge_scalar_like` instead of `merge_scalar`.

`depends_on` merges key-by-key too, not by the set-like lists' rule,
even though its surface grammar is still a reference list
(`depends_on [db]`): once an entry can carry a `condition`, two entries
naming the same service could genuinely disagree, so `hllc` keys the
merge on the referenced service's own name via a dedicated
`merge_depends_on`, not `LIST_FIELDS`'s distinct-name concatenation—see
#155. The service's own body still always wins over a template's entry
for the same dependency—but unlike `env`/`volume`/`publish`'s own
`merge_map`, `hllc` compares two explicit templates naming the same
service by their *effective* condition (a bare entry means the same
thing as an explicit `service_started`) before it calls anything a
collision: agreeing entries (including two plain `depends_on [db]`, by
far the common case) still silently collapse to one, exactly as they
did before #155 existed, while only two explicit templates whose
conditions genuinely differ raise the same `MapKeyCollision` two
explicit templates setting the same `env` key to different values
would. Treating mere agreement as an error would have been a gratuitous
breaking change to every `.hll` file already composing two templates
that each depend on the same service—the same reasoning
`hl-codegen`'s `AmbiguousExternalNetwork` check already applies to a
network named `external` twice: naming one thing more than once isn't
an ambiguity between it and itself, it's one answer given twice.

`router` merges by router name, and then *per sub-field* within
each name—the keyed form of the per-sub-field merge the next paragraph
describes for `expose`, one level deeper because a router's sub-fields
sit under a name rather than directly on the struct. Both levels are
load-bearing. Keyed, so two tiers naming different routers give a
service both rather than one. Per sub-field, so a service body writing
`router api { host: "..." }` over a template's `router api { entrypoint:
web-secure, path_prefix: [...] }` means "same router, different host"
rather than "throw the rest away"—the full-entry replacement `merge_map`
gives `volume`/`publish` would silently discard it, since a `volume`
entry has nothing inside it to keep and a router does. Within one name,
`host` is a scalar and collides between two explicit templates, while
`entrypoint` and `path_prefix` concatenate—`entrypoint` by distinct name
like `middleware`, `path_prefix` keeping duplicates like `dns`, since
prefixes are `||` alternatives whose written order is observable in the
emitted rule. A collision names the router as well as the field, through
the same `MapKeyCollision` a colliding `env` key raises, since a message
about `router.host` alone doesn't say *which* router—#184.

`expose` and `healthcheck`, the built-in struct fields
with more than one sub-field, both merge per sub-field (`expose`'s
`port`/`host`/`entrypoint` and `healthcheck`'s
`test`/`interval`/`timeout`/`retries`/`start_period`/`start_interval`/
`disable`) rather than as one
indivisible unit—the same key-by-key reasoning as a map field, applied
to a struct's named fields instead of a map's keys. Each sub-field then
merges by its own kind: `expose.port`/`.host` and every `healthcheck`
sub-field but `entrypoint` are scalars and collide (`healthcheck.test`
and `.disable` collide the same way even though neither is a `Literal`
—see below), `entrypoint` is a list and concatenates, so two explicit
templates each naming one entry point yield a router attached to both
rather than a `FieldCollision`. Two naming the same entry point yield a
router attached to it once, per the distinct-name rule. This means a
service's own body can override just `expose.host` (or just
`healthcheck.interval`) while still inheriting the rest from a
`with`-listed template, without repeating them. Two explicit templates
only collide if they set the *same* scalar sub-field, not merely the
same enclosing field overall.

`healthcheck.test`, whose type is `HealthcheckTest`, `healthcheck.disable`,
a bare-presence flag, and `privileged`, another bare-presence flag but a
field directly on `ServiceFields` rather than nested inside a struct,
aren't `Literal`s, so none of the three can ride the same name-keyed
`SCALAR_FIELDS` table `expose.port`/`.host`/`restart.policy` do—
`merge_scalar_like` in `compose.rs` is `merge_scalar` generalized over
the value type, applied to three dedicated `MergeAcc` slots instead of
more table rows, since only these three fields need it—see #153, #157.

`command`'s own type, `Command`, shares this same shell-string-or-
exec-list shape and merges through the same `merge_scalar_like`, with a
third dedicated `MergeAcc` slot of its own—see #156. It's the direct
`ServiceFields` case rather than the nested-struct one: `command` sets
`ServiceFields::command` straight from `merge_scalar_like`'s result, the
way `container_name`'s row in `SCALAR_FIELDS` sets its own field
directly, rather than reaching through a `get_or_insert` on an enclosing
struct the way `healthcheck.test` reaches into `Healthcheck` first.

`entrypoint`'s own type, `Entrypoint`, merges the same way `command`
does, in a fourth dedicated `MergeAcc` slot—see #183. It's a scalar-like
field, not a list field: a service's own `entrypoint` replaces an
inherited one outright rather than concatenating with it, and two
explicit templates that each set `entrypoint` collide. Replacing is the
only defensible rule here, since the value is one whole argument vector:
concatenating two exec lists would build a command line neither template
asked for. `entrypoint` and `command` hold separate slots, so a template
setting one and a template setting the other merge cleanly rather than
collide.

```
template internal_web(port: Number) {
  expose $port, entrypoint: web-secure
}

service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just expose.host—port and entrypoint still come from
  # internal_web above
  expose { host: "tools.internal.techdebtor.io" }
}
```

## Imports

Real-world templates and network declarations are for sharing across
many service files, not for copy-pasting into each one. `use` imports
another `.hll` file under a local alias. `alias.name` then references
anything that file declares at its top level.

```
use "docker.hll" as traefik
```

- `use`'s path is always a quoted `STRING`—`IDENT`'s grammar
  (`[A-Za-z_][A-Za-z0-9_-]*`) can't represent `.`/`/` at all, so a bare
  path isn't lexable. It's resolved relative to the *importing file's
  own location*, never the entry file's location or the directory the
  compiler ran from.
- `alias.name` qualifies any reference that would otherwise be a bare
  `IDENT`: a `networks [...]` entry (`networks [traefik.traefik-net]`),
  a named-volume mount's host side (`volume storage.media -> "/data"`),
  or a `with` invocation's target (`with common.internal_web { ... }`).
  `middleware`/`depends_on`/`dns`/`env_file` don't support a qualified
  form. None has a coherent cross-file meaning: `depends_on` names a
  same-file sibling service, and `middleware`/`dns`/`env_file` aren't
  resolved against anything an `.hll` file declares at all—an
  `env_file` entry names a path on disk next to the generated Compose
  file. `devices` isn't in this list any more either, but for a
  different reason—#167 made its entries plain literals now, like
  `publish`'s and `env`'s, which were never reference-list fields to
  begin with and so were never candidates for a qualified form in the
  first place.
- **Templates are lexically scoped, not dynamically scoped.** If a
  template declared in `templates.hll` writes
  `networks [traefik.traefik-net]`, that `traefik` resolves against
  *`templates.hll`'s own* `use` declarations—never whichever file
  happens to invoke the template with `with`. A template's references
  always resolve relative to where it was *written*, not where it was
  *called from*.
- **Imports aren't transitive.** `use`-ing a file only makes *that
  file's* own top-level declarations available under your alias—not
  anything *it* in turn `use`s. If `service.hll` uses `templates.hll`,
  and `templates.hll` uses `docker.hll`, `service.hll` can't write
  `docker.hll`'s alias itself. Only `templates.hll`'s own template
  bodies can, via the preceding lexical-scoping rule.
- **An imported network or volume keeps its own bare name**, and a
  service's `networks [...]` entries and named-volume mounts resolve
  against that bare name, so two of either can't share one. A file that
  pulls in `ext.proxy` while also declaring its own `network proxy`—or
  that pulls in both `a.proxy` and `b.proxy`—is a compile error rather
  than a silent pick between them, and `storage.media` against a local
  `volume media` is the same error on the volume side. Two files each
  declaring an unrelated `network proxy` stay legal. The error only
  fires when a qualified reference actually brings one across an import
  into the other's company.
- **`use` shares declarations, not services.** The compiler builds only
  the entry file's own `service` blocks. It parses one in an imported
  file, so duplicate names and syntax still get checked, and then drops
  it, since nothing resolves a service across files anyway. Likewise, it
  looks up the implicit `defaults` template only in the entry module. It
  has no invocation to carry an alias, so there is no
  `with common.defaults` to write, and an imported `defaults` applies to
  nothing. Both are warnings rather than errors—see the following
  Diagnostics section.

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

volume syncthing-config {}

template internal_web(port: Number) {
  networks [traefik-net]
  restart unless-stopped
  expose $port, host: "{{name}}.internal.techdebtor.io", entrypoint: web-secure
  middleware local-ipwhitelist
}

template authenticated {
  middleware forwardAuth-authentik
}

template linuxserver_app(puid: Number, pgid: Number) {
  env PUID = $puid
  env PGID = $pgid
}

service syncthing {
  with internal_web { port: 8384 }, authenticated, linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume syncthing-config -> "/config"
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

template internal_web(port: Number) {
  networks [net.traefik-net]
  restart unless-stopped
  expose $port, host: "{{name}}.internal.techdebtor.io", entrypoint: web-secure
  middleware local-ipwhitelist
}

template authenticated {
  middleware forwardAuth-authentik
}

template linuxserver_app(puid: Number, pgid: Number) {
  env PUID = $puid
  env PGID = $pgid
}

# syncthing.hll
use "templates.hll" as common

volume syncthing-config {}

service syncthing {
  with common.internal_web { port: 8384 }, common.authenticated, common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume syncthing-config -> "/config"
}
```

`syncthing.hll` never itself `use`s `network.hll`—only `templates.hll`
does—yet `internal_web`'s own `networks [net.traefik-net]` still
resolves correctly no matter which service ends up invoking it, since it
always resolves against `templates.hll`'s own alias table, never the
caller's.

A `with` list composing several templates reads as one long line once it
grows past two or three—per the preceding Syntactic grammar section, "a
trailing comma continues a comma-list," so the same `with` line can
instead wrap across multiple lines, one template per line, as long as
every line but the last ends with a trailing comma:

```
volume syncthing-config {}

service syncthing {
  with common.internal_web { port: 8384 },
       common.authenticated,
       common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume syncthing-config -> "/config"
}
```

This parses identically to the preceding single-line form—it's purely a
readability choice, not a different construct.

## Pipeline

1. **Lexer** (`crates/hl-lexer`)—one reserved word (`template`),
   string/number literals, `{`/`}`/`[`/`]`/`.`/`$`, `->`, `:`, `=`,
   `(`/`)`, `,`, and `#` line comments. Everything else is just an
   identifier to the lexer. Meaning comes from the schema table during
   parsing.
2. **Parser** (`crates/hl-parser`)—one generic block parser, not one
   function per keyword: parse `<type> [<n>]`, then a bare value or
   list—the primary-field shorthand—or a `{ field: value, ... }` body,
   recursing into nested blocks. A schema table drives both parsing
   and validation. Covers every built-in type (`network`, `service`,
   `image`, `expose`, `publish`, `volume` in both its top-level
   declaration and its `service`-field forms, `env`, `restart`, `raw`),
   full
   `template`/`with` composition, and `use`/alias-qualified references,
   see the preceding Composition and Imports sections—purely syntactic,
   no name resolution.
3. **Compose** (`crates/hl-parser`'s `compose` module)—resolves every
   `with`-list into a fully merged `Service` with no templates or
   unresolved parameters left, per the Composition section's 3-tier merge
   rules. Generalized over a `SymbolResolver` trait so the same merge
   engine resolves both a single file's own templates (`compose`, no
   imports) and a whole `use` graph (`compose_with_resolver`, driven by
   the linker below).
4. **Linker** (`crates/hl-linker`)—loads a `use` graph off disk (or, for
   tests, an in-memory map) into a module graph, and implements
   `SymbolResolver` over it so `compose_with_resolver` can resolve
   cross-file `alias.name` references—see the preceding Imports section.
5. **Codegen** (`crates/hl-codegen`)—walks a composed program and emits
   one Compose YAML document per input file (which may hold multiple
   services), with Traefik labels on each service's own `labels:` list.
   Codegen also hosts the two by-name reference checks, since each asks
   a whole-program question a single service's syntax can't answer. A
   `networks [x]` entry has to resolve to a top-level `network x`, or
   codegen reports `UnknownNetwork`. A `volume` entry whose host side is
   a named-volume reference, meaning an unquoted identifier rather than
   a quoted path, has to resolve to a top-level `volume x`, or codegen
   reports `UnknownVolume`. Every referenced declaration contributes its own
   entry, options included, to the document's top-level `networks:` or
   `volumes:` section, and neither section carries a declaration nothing
   references. Bind-mount paths pass straight through and need no
   declaration, exactly as Docker asks for none.

   `default` is the one exception to `UnknownNetwork`, and the one
   network name that gets special codegen treatment at all—every program
   has it whether or not it declares one. `networks [default]` with no
   matching `network default { ... }` resolves to Compose's own implicit
   default network rather than reporting `UnknownNetwork`, and
   contributes nothing to `networks:`, since Compose defines that network
   itself. On top of that, a program with two or more `service`
   declarations—already one Compose stack, one output document—
   implicitly attaches every service to `default` in addition to
   whatever it names explicitly, exactly the behavior Compose itself
   gives a project with no `networks:` key on any service. A
   single-service program gets no such attachment, since Compose's own
   default there is already implicit for free. An explicit `network
   default { ... }` declaration still wins over both of those: its
   `external`/`name` settings apply as they would to any other network,
   and it still emits its own `networks:` entry.
6. **Command-line tool** (`crates/hl-cli`, binary name `hllc`)—
   `hllc <file.hll>` lexes and prints tokens. `hllc --parse <file.hll>`
   parses and pretty-prints the Abstract Syntax Tree (AST). `hllc --build
   <file.hll> [--out <path>]` runs the full pipeline—link → Compose →
   codegen—and writes (or, with no `--out`, prints) the resulting Compose
   YAML. `--build` also accepts a directory, in either of two shapes:
   - **Flat**: every `.hll` file directly inside the directory is its own
     independent entry point with its own `use` graph, each writing to
     `<out>/<stem>/docker-compose.yml`. This shape requires `--out`—with
     potentially many files' output, there's no single meaningful default
     location.
   - **Co-located** (chosen automatically when the directory holds no
     `.hll` files of its own, but at least one immediate subdirectory
     that does): recurses exactly one level, and each such subdirectory's
     single `.hll` file builds in place, right back into that same
     subdirectory by default (`<subdir>/docker-compose.yml`)—no `--out`
     needed. An explicit `--out <dir>` still remaps the whole tree, the
     same way it does for the flat case, keyed by each subdirectory's own
     name (`<out>/<subdir-name>/docker-compose.yml`) rather than a file
     stem. This is the shape a real homelab tends to use in practice—
     `it_tools/it_tools.hll` alongside `it_tools/docker-compose.yml`,
     rather than every service's `.hll` file living in one flat
     directory—so a service's `.hll` source stays next to its other
     files (`.env`, bind-mounted config) instead of splitting them across
     two locations. A subdirectory with more than one `.hll` file is a
     hard error (ambiguous which one's output belongs directly in that
     subdirectory), not a silent guess.

   `--build` prefixes every document it emits, on any of those paths,
   including stdout, with a `# Generated by hllc` header. It's inert
   to Compose, it makes generated files self-identifying in a repo and in
   review, and it's what lets the compiler recognize its own previous
   output: before writing, `hllc` refuses any existing file that lacks
   that header, including a symlink whatever its target, unless the
   caller passes `--force`. The point is the incremental
   migration—converting one service to `.hll` while its neighbours stay
   hand-written Compose—in which co-located mode writes to paths it
   found by scanning rather than paths the user named, so an unguarded
   write silently destroys the hand-written files it happened to find.
   Rebuilding works fine: `hllc`'s own output already carries the
   header, so `hllc` overwrites it as before.

## Diagnostics

Most diagnostics are hard errors: a stage returns one, the pipeline
stops, and `hllc` exits non-zero having printed it to stderr. Every one
of them carries a span, and every span carries the identity of the file
it came from, so a location renders as `path:line:col` even when the
offending field came from a template in an imported file the user never
opened.

Alongside that, each stage accumulates **warnings**—non-fatal
diagnostics for constructs the compiler deliberately drops. Each stage
hands its warnings back with its success value
(`hl_linker::Linked::warnings`, `hl_codegen::GeneratedProgram::warnings`)
in the same shape errors render in, with a `warning:` marker after the
location. `hllc` prints them to stderr and touches neither its exit code
nor its output. Three constructs warn today: a `service` in a non-entry
file, a `defaults` template in a non-entry file, and a top-level
`network` no service references. That last one drops out of assembling
the `networks:` section from services' references, which leaves a
declaration nothing names with nowhere to go. An explicit `network
default { ... }` in a multi-service program doesn't trigger it, even
though no service writes `networks [default]` by hand: #152's
auto-attach counts as a reference for exactly this check. A top-level
`volume` no service mounts drops out of `volumes:` for the same reason,
but raises no warning yet.

The channel is deliberately minimal. Nothing promotes a warning to an
error, and there's no `--quiet`, `-W`, or `-A` style suppression yet.
Warnings are a named enum per stage precisely so a later suppression
scheme has something to filter on.

The fourth construct of this shape is *not* a warning: `middleware` or
`expose.entrypoint` on a service with no `expose.host` is a hard error.
Both fields only exist as labels on a Traefik router, and `expose.host`
is what creates that router, so no reading of the pair means anything.
Dropping them quietly, by contrast, shipped a service with its
forward-auth missing and nothing to say so.

`traefik`'s own `disabled` flag raises the mirror-image error—see
#159: setting `expose.host`, `expose.entrypoint`, or `middleware` on a
service that also switches Traefik off. Both diagnostics share one
shape, a field whose only meaning depends on a router existing,
contradicted by something else the same service says about that very
router, so this gets the same hard-error treatment rather than a fourth
warning. The router-less case is a missing piece silently completing
itself the wrong way. This one is a direct contradiction between two
things the author wrote on purpose, which reads as even less likely to
be an accident, not more. Letting the flag lose silently would keep a
router alive against the service's own stated intent. Letting the
router-attached field lose silently would build a compile-broken
deploy that looks fine until Traefik never picks it up, and neither
failure mode is one `hllc` should choose on the author's behalf.
`expose.port` is exempt: it's Compose's own `expose:` key, plain
container-network visibility with nothing to do with Traefik, so a
service with Traefik off may still declare one. That same flag conflict
covers a `router` block too—a whole router declared on a service
that just said it wants none is the same contradiction one step further
along.

`router` adds three more hard errors of its own, all of the same
shape—something that can only be a Traefik router, either contradicted,
or left incomplete—#184:

- A `router` block with no `host` has no rule to emit, so there is
  nothing it could have meant. This is stricter than `expose`, which
  tolerates a host-less block because its `port` still does a second job
  (Compose's own `expose:` key) that has nothing to do with Traefik. A
  `router` has no second job.
- A service that sets both `expose.host` and an *unnamed* `router { }`
  has two blocks claiming the router id `traefik.http.routers.<service>`,
  so one would silently overwrite the other's labels. Naming the block
  gives it its own id.
- A router name outside `[A-Za-z0-9_-]` draws a rejection. This is a
  different check from the metacharacter guard that covers label
  *values*, and deliberately a different character set: the name goes
  into the label **key**, `traefik.http.routers.<name>.rule`, where a `.`
  extends the dotted key and an `=` ends it outright, since Docker splits
  a label string on its first `=`. A bad name doesn't corrupt one label's
  meaning, it writes a different label. The grammar already refuses such
  a name—a router name is an `IDENT`—and codegen checks it again anyway,
  so its safety doesn't rest on the grammar staying as it stands.

The parser catches two `router` blocks in one body claiming the same
router id instead, alongside the other duplicate-key errors, since both
spans are still in hand there.

## Future work

- **`bootstrap` scaffold**—generate a brand-new homelab's starting
  `.hll` files from a template: a `docker.hll` declaring the shared
  network plus a `traefik` service (HTTPS termination, a
  certificate-resolver placeholder, the `web-secure`/`web` entrypoints),
  and a `templates.hll` with common reusable templates in the same shape
  as this doc's own worked examples—so starting a new homelab doesn't
  mean hand-writing the reverse-proxy service from scratch. Not yet
  designed: exactly where the line falls between what's generic enough to
  belong in the scaffold (entrypoints, the shape of a certificate
  resolver) versus what's homelab-specific and should stay a
  fill-in-the-blanks placeholder (DNS provider/credentials, domain, IP
  ranges)—see the preceding section, "Design principle: generic core,
  specific templates."
- **`hllfmt`**—an auto-formatter that would wrap a long `with` list past
  some line length (see the preceding multiline `with` example) with
  consistent indentation, instead of that being a manual per-file
  judgment call. Not yet designed: the line-length threshold, and whether
  formatting stays opinionated and non-configurable (à la
  `gofmt`/`rustfmt`) or takes any settings at all.
