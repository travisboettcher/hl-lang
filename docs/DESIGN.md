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
  else that looks like a keyword (`service`, `network`, `image`, `build`,
  `volume`,
  `publish`, `env`, `env_file`, `restart`, `expose`, `healthcheck`,
  `depends_on`, `networks`, `dns`, `devices`,
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

param          ::= IDENT

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

literal        ::= STRING | NUMBER | IDENT | IDENT "." IDENT | "$" IDENT

rule_expr      ::= or_expr

or_expr        ::= and_expr ( "||" and_expr )*

and_expr       ::= unary ( "&&" unary )*

unary          ::= "!" unary | primary

primary        ::= "(" rule_expr ")" | matcher

matcher        ::= IDENT "(" ( literal ( "," literal )* )? ")"
```

- `rule_expr` is reachable from exactly one place: `router`'s own `rule`
  field, whose schema row `schema::FieldKind::MatchExpr` is the switch
  that routes the generic engine into this grammar rather than into
  `value`. It's the only field kind whose value is neither a literal, a
  list of them, nor a nested body, and the only one with an operator
  grammar. Precedence is Traefik's own—`!` tightest, then `&&`, then
  `||`—which is what lets codegen render a parsed tree back out without
  parenthesizing defensively: an expression written without parentheses
  reparses in Traefik as the tree it parsed as here.

  It's also the language's **second** self-recursive production, after
  `raw`'s schema-free value grammar, so it carries a depth ceiling for
  that one's reason, per #72: unbounded, a few kilobytes of `((((...))))`
  overflows the stack, and a stack overflow aborts the process rather
  than returning an error a library embedder can catch. The two
  constants sit beside each other, `MAX_MATCH_EXPR_DEPTH` and
  `MAX_RAW_VALUE_DEPTH`, and the second's own rustdoc carries the
  measurement behind both numbers. The depth
  counted is the parsed *tree*'s rather than the parser's call chain, so
  a long `a && b && c && ...` chain counts against it as well as a stack
  of `(`: `&&` folds left, so each extra operand is one more `Box` level
  for drop glue to walk.

  One token of lookahead decides where a `rule` expression *ends*, since
  the lexer emits no newline token. It stops the moment the next token
  is neither `&&` nor `||`, and the parser takes a `,` only between a
  matcher's own parentheses—never at the top level—so in the
  comma-continued form `router api, rule: Host("x"), entrypoint: web`
  the second comma starts a sibling field of `router`, exactly as it
  would after any other field's value. The `(` of a `param_list` never
  competes with the `(` of a `matcher` or a group: `param_list` is
  reachable only after the `template` keyword at the top level.
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
- A parameter carries no type annotation at all, per #201: `param` is
  just `IDENT`. Earlier milestones let it optionally declare `Number` or
  `String`, checked strictly against the argument's own literal kind at
  the call site. That system had a ceiling this doc used to name here: a
  bare-`IDENT`-typed or list-typed parameter wasn't expressible, so
  `networks [$net]`'s `net` got typed `String` for lack of anything
  closer, even though a network name can just as legally take the form
  of a bare identifier. #196 made the gap impossible to ignore: once
  `$param` could reach every reference and list position a plain literal
  could, `Number`/`String` covered less and less of what a parameter's
  declaration site could actually receive. The fix wasn't a third and
  fourth type name—a reference type, a list type—grafted onto the
  annotation grammar. That would have made the annotation duplicate
  information the field's own schema already carries structurally
  (`schema::FieldKind::ReferenceList` versus a plain `Scalar`, and the
  handful of fields `book/src/built-in-fields.md`'s "Accepts" column
  documents as `number`), and kept duplicating it every time a new field
  kind arrived, with nothing forcing the two descriptions to agree.
  Dropping the annotation and checking a substituted argument against
  the field it lands in instead needs no such vocabulary. A
  reference-shaped position (`networks`, `dns`,
  `env_file`, a `depends_on` entry's own reference, `expose.entrypoint`,
  `router.entrypoint`, `router.path_prefix`, `router.middleware`, a
  `router.rule` matcher's own arguments) rejects
  a substituted `Literal::Number`—the one literal kind that position's own grammar
  (`parse_literal_reference`) can never produce directly. A
  `number`-typed position—`expose.port` and `healthcheck.retries`, the
  book's own two `number` rows—rejects a substituted argument that isn't
  one, the same way. Every other position accepts any literal kind,
  exactly as writing it there directly already does. Both checks name
  the offending argument at its own call site, not the `$param`
  reference inside the template body, for the same reason: substitution
  overwrites the whole `Literal` slot, span included, with the caller's
  own literal, so that span is what a mismatch reports (see
  `compose::ComposeError::ArgumentNotReferenceShaped`/
  `ArgumentNotNumeric`).
  The numeric check goes one step further than a substitution-time check
  alone could: `expose.port`/`healthcheck.retries` written directly—by a
  plain service, or inside a template's own body with no `$param` in
  sight—get the same rejection from a second, backstop check
  (`ComposeError::FieldNotNumeric`) run once on each service's fully
  merged fields, since a hand-written mismatch never passes through
  substitution at all for the first check to see. That makes this
  strictly stronger than the annotation it replaced: `: Number` was a
  per-template opt-in a plain service's own `expose "eight-thousand"`
  could never reach, while the backstop checks every service, whether it
  uses a template or not. The one thing dropping the annotation gives up
  is a check at
  the parameter's declaration site regardless of where the argument ends
  up: a parameter that never lands in a reference-shaped or
  `number`-typed position goes unchecked, the same as an untyped one
  always did, since there's no field-shape left to check it against.
- The `"$" IDENT` form of `literal`, a parameter reference such as
  `$port`, is only legal inside a `template`'s own body—including a
  nested `with`-invocation argument body written inside that template,
  where a `$name` forwards the *enclosing* template's own parameter (see
  Composition, below). Used anywhere else (a plain `service`/`network`
  body, or a `with`-invocation body written inside one of those), it's a
  compile error: only a template body has a declared parameter list to
  resolve `$name` against. This is, like the preceding newline/comma
  layout rules, a context-sensitive constraint the plain grammar can't
  express. The check is uniform across every position `literal` appears
  in, `networks`/`dns`/`env_file`/an `entrypoint` or `middleware`
  list/a
  `depends_on` entry included—a template can write `networks [$net]` or
  `router { middleware: $mw }` exactly as freely as `restart $policy`, since there is
  only the one grammar production for a value everywhere it's expected.
- The `IDENT "." IDENT` form of `literal`—`alias.name`, produced only
  from a bare `IDENT` token followed by `.` `IDENT`, never from a
  `STRING`—is what qualifies a reference against a `use`-imported file's
  local alias—see the following Imports section. Every position `literal` appears in
  parses it the same way, but whether it's *semantically* legal there is
  a separate, per-field question: `networks` and a named-volume mount's
  host side resolve it against the aliased file's own declarations,
  while every other position rejects it outright, since none of them
  names something an `.hll` file declares in the first place.

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
3. **Secondary-field bare shorthand**—after a primary value (or a
   repeatable struct field's own name, for `router`), a type's schema
   drives one further generic continuation: additional explicit `key:
   value`/`key` fields may follow, each preceded by a **mandatory
   comma** (the same "trailing comma continues, its absence ends the
   statement" rule as any other comma-list): `router api, host: "...",
   entrypoint: web`. A field whose own value is an unbracketed
   comma-list (`entrypoint`'s reference list) ends at the next `key:`
   rather than swallowing it, by the same one-token lookahead: in
   `router api, entrypoint: web, host: "..."`, the second comma starts a
   sibling field of `router`, not a second entry point.
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

   `expose <port> as "<host>"` looks like a special case of this same
   rule—fusing directly onto the primary value with **no comma**—and
   through #197 it was one: `TypeSchema::bare_keyword_alias`, generic
   schema data pairing a bare keyword (`as`) with a target field
   (`expose`'s own `host`). #198 moved every Traefik-routing field off
   `expose` and onto `router`, leaving `expose` with only `port`—no
   field left for a generic alias to target—so `as` survives as bespoke
   parser sugar instead (`Parser::parse_expose_as_sugar`), outside this
   generic engine entirely: it desugars `expose <port> as "<host>"` to
   `expose { port }` plus an unnamed `router { host }`, the two parsed
   nodes a hand-written pair would produce. It keeps the one property
   that mattered about the old mechanism: a one-shot continuation, not a
   list. No further field can follow it, comma-led or bare. `expose port
   as "host", entrypoint: web` is a compile error, though
   no longer a dedicated one: `ParseError::AliasSugarCannotContinue`
   doesn't exist any more, so the trailing comma is simply left for the
   enclosing
   body, where a bare comma never starts a valid statement.
4. **Repeatable-field accumulation**—semantic, not part of the
   Context-Free Grammar (CFG)—writing `volume`, `publish`, `env`,
   `depends_on`, or `router` more than once in
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
| `raw` | map |—| `:` | key | no |

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

`raw`'s key-side uniqueness follows `env`'s rule as of #193: two
*explicit* `with`-listed templates setting the same `raw` key raise the
same `MapKeyCollision` a repeated `env` key does, rather than the second
template's value silently overwriting the first's. Before #193, `raw`
was the one field the merge concatenated outright at every tier, with no
uniqueness check at all—the escape hatch was, ironically, the one place
composing two templates could lose a value in silence. This check is a
*composition*-level statement, not a parser one: the parser still never
checks a `raw` body's own entries against each other, so a key repeated
within one body—one `raw { }` block or two in the same `service`/
`template`—stays unchecked, exactly as before #193.

Codegen has a separate rule about `raw` keys, unaffected by #193, because
YAML forces the issue—two keys spelled the same in one mapping is
invalid—and a `raw` key may well name a field that has a preceding row.
Where it does, codegen emits the `raw` value and drops the built-in one,
which keeps adding a row to this table from breaking files that were
already reaching for `raw` in that row's absence (see the `raw` section
of the book's Built-in Fields page).

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

`build` is the newest row, added by #224, and the one that graduated
out of `raw` for a reason none of the preceding ones had. The others were
promotions of convenience. `raw` could already write
`dns`/`env_file`/`healthcheck`/`privileged`/`devices` perfectly well,
and each got a row because a generic Compose key deserves validation and
a spelling of its own. `build` had no spelling at all. Codegen demanded
an `image`, and checked that against the structured `image` field, so a
service built from a local Dockerfile, which has no `image` and no
reason to invent one, failed to compile whatever it wrote, `raw`
included. That made `build`'s absence a hole in the language rather than
a gap in its convenience, which is what moved it ahead of the rest of
the long tail.

The fix has two halves, and the second matters as much as the first.
Adding the row is the obvious half. The other is that "does this service
have an image" stopped being a question about a *field* and became one
about the emitted *document*: codegen now asks whether the finished
service block carries an `image:` or a `build:` key, from any source,
once `raw` overrides have had their say. Those two questions had quietly
come apart in two directions. A `raw { image: "..." }` that supplies the
key by hand drew a refusal even though the key it writes is exactly the
key codegen demands, and a service built from a local context drew one
for lacking a key it correctly never sets. Asking the document answers
both at once, and it's the question codegen actually cares about: what
matters is that Compose gets something to run, not which `.hll`
construct produced it. `apply_raw_overrides` already assumed `raw`
stands in for a built-in field, and this makes the image requirement
assume the same thing.

`build` carries `context` plus `dockerfile`. `context` is the primary
field for `image`'s `ref` reason: one bare value stands in for the whole
struct, and Compose has its own short form spelling exactly that.
`build` emits whichever of Compose's two shapes the source implies, the
bare context string unless a `dockerfile` forces the mapping. `args` is deliberately absent,
being a map that would need the merge machinery `env` has rather than
the two plain scalars here, with nothing yet needing one. A `build`
block with no `context` is a hard error rather than a silent default to
`.`, for `router`-without-`host`'s reason one level over: a wrong
default here builds the wrong directory rather than nothing.

`publish` and `expose` are separate rows on purpose, not two spellings
of one concept. `publish` is Compose's `ports:` key, which puts the port
on the Docker host where the local network can reach it, and `expose` is
Compose's `expose:` key, which reaches only other containers on the same
network. A homelab needs both: much of it sits behind Traefik and wants
`expose`, while Pi-hole on 53, a Syncthing sync port, or a game server
takes traffic directly and wants `publish`—see #84. `publish`'s own
uniqueness lands on the container port rather than the host one because
a protocol suffix rides on the container half of a Compose short-syntax
mapping (`53:53/udp`), so checking the host side would reject one host
port serving both protocols—exactly the configuration the field exists
to express.

Through #197, `expose` also modeled exactly one Traefik router of its
own: `host` generated the router-rule label, and `entrypoint` (a list of
references, `entrypoint web, web-secure`, for the same reason
`router`'s own `middleware` is—Traefik's `entrypoints=` label is comma-separated, and
modelling that as a list keeps the separator codegen's to write rather
than the user's) restricted it to named entry points. #198 moved both
fields onto `router` outright, leaving `expose` with the one field it
still has, `port`—see that field's own paragraph below.

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

`router` is the one row that's both repeatable *and* struct-kind, and
the only one whose instances take a key from a name the user writes. It
started as the second way to get a Traefik router—#184—for the
service that needs more than one off one container: a public host beside
a local-network host, an API path prefix split off from a catch-all
frontend. Before #184 the only way to say that was to abandon `expose`
and hand-write the whole label list in `raw { labels: [...] }`, which
gives up `expose`'s validation, its `{{name}}` interpolation, and its
metacharacter guard for a service that's otherwise an ordinary
Traefik-fronted container. #198 then moved every Traefik-routing field
off `expose` and onto `router` outright, so today `router` is the *only*
way to get one—the unnamed `router { }` form (or its `expose <port> as
"<host>"` sugar) is simply the common single-router case, not a
different mechanism from the multi-router one.

`port` stays on `expose` rather than moving alongside `host`/
`entrypoint`: it's per Compose *service* rather than per router—several
routers off one container all balance onto that one port—and `expose`
would need it regardless of Traefik for its own `expose:` key, so giving
`router` a repeatable field that's never actually per-router would have
been the wrong shape. See #198's "port" paragraph below.

Each block emits `traefik.http.routers.<service>-<name>` for its labels,
or `traefik.http.routers.<service>` for the unnamed `router { }` form.
Two blocks in one body claiming the same id—whether both are hand-written,
or one is hand-written and the other comes from `expose <port> as
"<host>"`'s own sugar—is a compile error rather than one silently
overwriting the other's labels, the same rule two `volume` entries at
one container path already follow: that isn't two routers, it's one
router described twice. The parser catches this directly—see
Diagnostics, below—since both spans are still in hand there. #198
folded what used to be a separate codegen-level check,
`ExposeHostWithUnnamedRouter`, for the one case reachable when `host`
still lived on `expose`, into this same parse-time duplicate check, now
that both an explicit unnamed `router { }` and the `as` sugar's own
unnamed router are ordinary entries in one list before composition ever
runs.

`host` is a plain scalar. `entrypoint` and, since #221, `middleware`
are reference lists, spelled and merged the same way `networks` is—see
`middleware`'s own paragraph further down. `path_prefix` uses that same
reference-list grammar too: a prefix is free text a template
legitimately fills in with a `$param`, which every reference-list field
accepts—`entrypoint` included—since `literal` now carries `$param`
itself rather than needing a separate grammar to hold it. The qualified
`alias.name` form still doesn't reach `path_prefix`'s generated output,
though: it parses there like anywhere else, but `path_prefix` rejects
it, the same as `dns`/`env_file`/`entrypoint`/`middleware`/`depends_on`—see
the following Imports section. With prefixes set, the rule
becomes ``Host(`h`) && (PathPrefix(`a`) || PathPrefix(`b`))``. The
parentheses are load-bearing, not cosmetic: `&&` binds tighter than `||`
in Traefik's rule grammar, so without them the rule would match *any*
host under the last prefix. They're emitted for a single prefix too,
where they change nothing, so the rule's shape doesn't depend on how many
prefixes it happens to have.

`router` carried no `port` through #224, on the reasoning that
Compose's `loadbalancer.server.port` label is per Compose service rather
than per router: a container listens on one port however many routers
point at it. That's true of nearly every service, and false of
`sftpgo`, which serves a web UI on 2222, WebDAV on 4444, and raw SFTP on
1111 off one container. #225 added the `port` sub-field for that case.
A router naming one gets a Traefik *service* of its own, keyed by the
router's own id, and says so with a `.service=` label. A router naming
none still falls through to the single service-wide target `expose`
supplies, which is what keeps every file written against the older model
emitting exactly what it always did.

That splits the "a routed service needs a port" rule in two, and the
split is the interesting part. `expose` is now required only while at
least one router actually falls back to it. A service whose routers all
name their own needs no `expose` at all, and demanding one would mean
inventing a "the" port that `sftpgo` genuinely doesn't have, the very
reason its `expose:` key stayed in `raw` alongside its labels. The
diagnostic points at the falling-back router rather than the first one,
since that's the block the fix belongs to.

`priority` is the smallest of #225's three additions and needs least
justification: a plain generic Traefik router setting, emitted verbatim
as a number, absent by default so Traefik keeps its own rule-length
heuristic rather than one `hllc` invented. What makes it necessary
rather than merely nice is that two routers sharing a host have nothing
else to tell them apart, which is exactly `sftpgo`'s `web`/`webdav`
pair.

`protocol` is the largest, since a TCP router isn't an HTTP router with
a flag on it. It emits `traefik.tcp.routers.*`/`traefik.tcp.services.*`,
a separate namespace, and matches ``HostSNI(`...`)`` rather than
``Host(`...`)``. At that layer there is no HTTP request to read a
`Host` header from, only the TLS handshake's server name, which is also
why `HostSNI` accepts a `*` wildcard that `Host` has no equivalent of.
Every other label a router emits keeps its spelling either side, which
is why one code path emits both, parameterized by the namespace segment
and the host matcher.

Two constraints follow, both hard errors rather than silent drops. A TCP
router can't take a `path_prefix`, having no request URI to match a path
against, and ignoring one would route traffic the block plainly meant to
narrow. And a TCP router must name its own `port`: the shared fallback
target is an *HTTP* service (`traefik.http.services.<service>`), so
there is nothing there for it to fall back to.

Codegen validates `protocol` rather than the parser, unlike
`depends_on`'s condition. The reason is `$param`: substitution runs
after parsing, so a parse-time check would see `$proto` unresolved and
reject it as an unknown protocol, which names the wrong problem
entirely. By the time codegen runs, composition has bound every
parameter, so what reaches the check is what the user actually wrote. `priority` and `port`
are numbers and ride the same `check_numeric_fields` backstop
`expose.port` already had, catching a hand-written mismatch that never
passes through substitution for the argument-side check to see.

`middleware` sits on `router` since #221, and only there—it used to be
a service-level field instead, one list attaching to every router the
service had. That issue is what settled the scope: a real service
(`gitea`) needs a public router on `git.techdebtor.io` with no
middleware beside an internal one on `git.internal.techdebtor.io` behind
`local-ipwhitelist`. With one service-wide list, either both routers get
the allowlist—breaking the intentionally public route—or neither does,
dropping IP restriction from the internal-only one. That isn't a style
difference, so the whole `labels` list stayed hand-typed in `raw`,
giving up every check `router` performs, for the one service that most
needed them.

The part worth justifying is moving the field, rather than adding a
per-router one beside it and keeping both, which would have been the
smaller change. Two spellings of one concept would need a precedence rule
between
them—override or extend, and either answer is wrong somewhere. Extend
leaves the preceding public router unable to shed the allowlist, which
leaves the reported gap unfixed. Override fixes that but makes the
service-level line mean "unless a router disagrees," so one `router`
block no longer states what that router attaches: you have to check the
service body too, and a middleware silently added to a public route or
silently dropped from a restricted one is a security bug that compiles
clean. A middleware is per-router in Traefik's own model, so the field
belongs on the router—one scope, read where it's written.

The cost is real and accepted: routers that share a middleware each name
it, where one service-level line used to do. Templates absorb most of
that—a template's own `router` block carries the shared name once for
every service that composes it—and what's left is repetition the
compiler can see, rather than brevity that hides which routers differ.

Across tiers—`defaults`, each `with` target, the service body—one
router's `middleware` concatenates and dedupes by name, exactly like its
`entrypoint`, so a template supplies a base list a service body adds to.

The old spelling doesn't silently vanish. `middleware` written in a
`service`/`template` body is `ParseError::MovedField`, naming where the
field went, rather than the generic `UnknownField`—which on these two
types offers the `raw { ... }` escape hatch, advice that here compiles
and then emits a meaningless `middleware:` Compose key while the Traefik
label the author wanted goes missing. That's the same "valid output,
wrong service" failure #144 closed off, arrived at through a helpful
hint, so the removed name stays recognized purely to name where it went
(`schema::moved_field`).

`rule` is the field that stopped `router` being able to express
only one shape of rule, per #228. `host` and `path_prefix` between them say
exactly one thing—a host match, with the prefixes joined by `||` and
hung off the host with `&&`—and #228 needed that shape's *inverse*: `adventure_log`
splits one host across two containers by path, the backend catching
four prefixes and the frontend catching everything else. The backend
half is the `||` shape `path_prefix` already produces. The frontend half
had no representation at all, so its whole `labels` list stayed in
`raw`, which is the same failure `middleware` had one paragraph up: the
one service that most needed `router`'s checks was the one that couldn't
use it.

The issue proposed a `negate` flag beside `path_prefix`. The reason that
was the wrong shape rather than merely a small one is that it buys
exactly one more rule. The next router wanting a header split, a method
match, or an `||` at the top level needs a second flag, and the one
after that a third, each with its own interaction with the others to
specify. An expression buys all of them at once and specifies nothing
extra, because Traefik has already specified it.

A raw rule *string* would have been simpler still, and it loses too
much. Splicing user text straight into the label loses the
backtick guard #65 put on `host`—a backtick has no escape inside
``Host(`...`)``, so one in a rule closes the matcher and writes a
second—and loses `{{name}}` resolution, matcher and arity checking, and
any span inside the rule to point a diagnostic at. Parsing the
expression is what buys all four, and anyone who genuinely wants
unchecked passthrough already has `raw`.

The matcher names are Traefik's own, spelling, capitalization, and all,
against the rest of the language's snake_case. A rule is a
thing users copy out of a Traefik label or the Traefik documentation, so
a renamed vocabulary would put a translation step in front of the one
operation this field exists to make easy. `hll`'s contribution is the
quoting: Traefik delimits an argument with a backtick, `hllc` writes
those, and the user writes an ordinary `"..."` string.

`host`/`path_prefix` survive rather than giving way to it, since the
single-host router is the overwhelmingly common case and deserves its
one-liner—but they survive as *sugar*, which `labels::sugar_expr` lowers
into the same `MatchExpr` a written-out `rule` parses to. One rule-rendering
path, not two that could disagree about escaping, interpolation, or
parenthesization. Writing both on one router is a hard error rather than
a precedence rule, for the reason the `middleware` move rejected one:
either answer silently drops something the block plainly meant.

`MatchExpr::Group` is a real node rather than something the renderer
infers from precedence, which is what keeps the sugar's output
byte-identical to what it always was. `path_prefix` deliberately
parenthesizes even a single prefix—where the parentheses change
nothing—so that a rule's shape doesn't depend on how many prefixes it
happens to have, and a precedence-only renderer would drop exactly
those. Keeping written parentheses also means a rule renders the way
someone typed it, so the source spells out the emitted label.

The parse-time/codegen-time split follows `protocol`'s own reasoning
rather than contradicting it. Which matchers exist and how many
arguments each takes can't depend on anything composition does—a
matcher name is an `IDENT`, which no `$param` can be, and substitution
replaces one literal with one literal rather than expanding a list—so
the parser checks both, where the span covers the matcher itself. Which *namespace* a matcher is legal in does depend on
`protocol`, so that check sits in codegen beside the rest of the
protocol-dependent ones, and runs both ways: `PathPrefix` under
`protocol: tcp` has no request URI, and `HostSNI` under `http` has no
TLS handshake.

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

`depends_on`, `networks`, `dns`, and `env_file` aren't
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
`volume` already does. `router`'s own `entrypoint` sub-field is a
reference list of Traefik entry-point names, and the new one is a
service-level command override. The grammar stays unambiguous for
exactly the reason `volume`'s two roles do: the parser resolves a field
name only through `schema::resolve_field` against the enclosing type's
own field list, so `entrypoint` written in a `service`/`template` body
reaches `SERVICE_FIELDS`'s row and `entrypoint` written inside a
`router { }` body—or after a named `router`'s own comma, where rule 3's
one-token lookahead consults `ROUTER`'s field list and nothing else—
reaches `ROUTER`'s. The parser consults neither table in the other's
position, and neither role involves a lexer-level keyword. The layout
rules keep the two apart at the statement level too: a struct-kind body
separates fields with a newline, so a service's own `entrypoint`
statement always begins a new statement rather than continuing a
preceding `router` shorthand, which only ever continues past a comma.

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
(`networks` alone, since #221 moved `middleware` onto `router`, where
`merge_routers` dedupes it by this same rule) concatenate by
*distinct* name, keeping the first occurrence, while `dns` and
`env_file` keep duplicates since their order is observable—resolver
priority for `dns`, Compose's own last-file-wins precedence for
`env_file`—see #154. Map fields merge key-by-key, or value-by-value for
`volume`, `publish`, and `devices`, and scalar fields (`image`,
`restart`, `expose`'s own `port`) error on collision among explicit
templates only. `devices`
used to sit in the set-like group too at #157, giving a repeated
`"host:container"` mapping the same first-occurrence-wins treatment a
repeated `networks` entry got, since there was no
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
through the same table-driven `SCALAR_FIELDS`/`merge_scalar` every
other scalar field uses.

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
describes for `healthcheck`, one level deeper because a router's
sub-fields sit under a name rather than directly on the struct. Both
levels are load-bearing. Keyed, so two tiers naming different routers
give a
service both rather than one. Per sub-field, so a service body writing
`router api { host: "..." }` over a template's `router api { entrypoint:
web-secure, path_prefix: [...] }` means "same router, different host"
rather than "throw the rest away"—the full-entry replacement `merge_map`
gives `volume`/`publish` would silently discard it, since a `volume`
entry has nothing inside it to keep and a router does. Within one name,
`host` is a scalar and collides between two explicit templates, while
`entrypoint` and `path_prefix` concatenate—`entrypoint` by distinct name
like `networks`, `path_prefix` keeping duplicates like `dns`, since
prefixes are `||` alternatives whose written order is observable in the
emitted rule. A collision names the router as well as the field, through
the same `MapKeyCollision` a colliding `env` key raises, since a message
about `router.host` alone doesn't say *which* router—#184.

`healthcheck`, the built-in struct field
with more than one sub-field, merges per sub-field
(`test`/`interval`/`timeout`/`retries`/`start_period`/`start_interval`/
`disable`) rather than as one
indivisible unit—the same key-by-key reasoning as a map field, applied
to a struct's named fields instead of a map's keys. Each sub-field but
`test` is a plain scalar and collides (`.test` and `.disable` collide
the same way even though neither is a `Literal`—see below). This means
a service's own body can override just `healthcheck.interval` while
still inheriting the rest from a `with`-listed template, without
repeating them. Two explicit templates only collide if they set the
*same* sub-field, not merely the same enclosing field overall—`expose`'s
own `port` follows the plainer, single-field version of this same rule,
listed among the preceding ordinary scalar fields now that #198 left it
`expose`'s only field.

`healthcheck.test`, whose type is `HealthcheckTest`, `healthcheck.disable`
and `privileged`—two bare-presence flags, `privileged` a field directly
on `ServiceFields` rather than nested inside a struct—and
`command`/`entrypoint`, whose types `Command`/`Entrypoint` each carry
Compose's own shell-string-or-exec-list pair of shapes, aren't plain
`Literal`s. None of these six can hold a bare `Literal` the way
`expose.port`/`restart.policy` do. `compose.rs` used to give
each of them its own `MergeAcc` slot and route it through a second
generic function, `merge_scalar_like`—`merge_scalar` generalized over
the value type, kept separate since only these six fields needed it.
#197 folded that second function and all six slots back into
`SCALAR_FIELDS` itself: each row's value is a `ScalarValue`, an enum
with a `Literal` arm for the ordinary case, a `List` arm for the
shell/exec pair's exec form (the shell form still rides `Literal`), and
a `Flag` arm for a bare-presence field's own span—its only "value."
`HealthcheckTest`/`Command`/`Entrypoint` stay separate types—they're
three different Compose keys, and collapsing them would blur that—but
they convert to and from the same pair of `ScalarValue` arms in each
row's own `take`/`set`. `merge_scalar` and `MergeAcc::into_service_fields`
stay the two generic loops #28 established. None of the six needs a
bespoke field or a second merge function any more. `command` sets
`ServiceFields::command` straight from its row's merged value, the way
`container_name`'s row already does, rather than reaching through a
`get_or_insert` on an enclosing struct the way `healthcheck.test`'s row
reaches into `Healthcheck` first.

`SCALAR_FIELDS` places `healthcheck.test` and `.disable` right after
`healthcheck`'s five plain-`Literal` rows, and `.disable` after `.test`:
a row's `get_or_insert` only stamps a freshly materialized struct's span
when nothing earlier in the table already did, so the most specific
sub-field present wins the cosmetic span. This ordering is explicit in
the table itself, not an accident of hash iteration—`SCALAR_FIELDS` is
an ordered list, not a map, precisely so this preference stays a stable
function of source order. `expose` no longer has a same-struct sibling
to race against for this: #198 left `port` its only field, so its own
row's `get_or_insert` always stamps the span it would have stamped
anyway.

`entrypoint` is a scalar-like field, not a list field: a service's own
`entrypoint` replaces an inherited one outright rather than
concatenating with it, and two explicit templates that each set
`entrypoint` collide. Replacing is the only defensible rule here, since
the value is one whole argument vector: concatenating two exec lists
would build a command line neither template asked for. `entrypoint` and
`command` are two separate rows keyed independently, so a template
setting one and a template setting the other merge cleanly rather than
collide.

```
template internal_web(port) {
  expose $port
  router { entrypoint: web-secure }}

service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just the unnamed router's host—port and entrypoint still
  # come from internal_web above
  router { host: "tools.internal.techdebtor.io" }
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
- `alias.name` qualifies any reference: a `networks [...]` entry
  (`networks [traefik.traefik-net]`), a named-volume mount's host side
  (`volume storage.media -> "/data"`), a `with` invocation's target
  (`with common.internal_web { ... }`)—and, syntactically, every other
  reference-shaped position too (`dns`, `env_file`, an
  `entrypoint` list, `depends_on`, `router.path_prefix`,
  `router.middleware`), since `alias.`
  and `$param` are the same `literal` production wherever it's
  written—see the preceding Syntactic grammar section. Only `networks` and a
  named-volume host actually *resolve* one, though: they're the two
  positions with a real cross-file declaration to resolve a qualifier
  against. Every other reference-shaped position rejects one outright,
  with `UnsupportedQualifiedReference`, exactly as it always has—none of
  them has a coherent cross-file meaning: `depends_on` names a same-file
  sibling service, and `dns`/`env_file`/an `entrypoint`
  list/`path_prefix`/a router's own `middleware` aren't resolved against
  anything an `.hll` file declares at all—an `env_file` entry names a path on disk next to the
  generated Compose file. `devices` was never a candidate for a
  qualified form in the first place—#167 made its entries plain
  literals, like `publish`'s and `env`'s, neither of which is
  reference-shaped either.
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

template internal_web(port) {
  networks [traefik-net]
  restart unless-stopped
  expose $port
  router {
    host: "{{name}}.internal.techdebtor.io"
    entrypoint: web-secure
    middleware: local-ipwhitelist
  }
}

template authenticated {
  router {
    middleware: forwardAuth-authentik
  }
}

template linuxserver_app(puid, pgid) {
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

template internal_web(port) {
  networks [net.traefik-net]
  restart unless-stopped
  expose $port
  router {
    host: "{{name}}.internal.techdebtor.io"
    entrypoint: web-secure
    middleware: local-ipwhitelist
  }
}

template authenticated {
  router {
    middleware: forwardAuth-authentik
  }
}

template linuxserver_app(puid, pgid) {
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
6. **Command-line tool** (`crates/hl-cli`, binary name `hllc`)—four
   subcommands, one per pipeline depth, each taking one positional path.
   `hllc tokens <file.hll>` lexes and prints tokens. `hllc parse
   <file.hll>` parses and pretty-prints the Abstract Syntax Tree (AST).
   `hllc build <file.hll> [--out <path>]` runs the full pipeline—link →
   Compose → codegen—and writes (or, with no `--out`, prints) the
   resulting Compose YAML. `hllc check <file.hll>` runs that same
   pipeline and writes nothing at all, exiting 0 or non-zero: the CI
   gate. Bare `hllc` prints help and exits 2, compiling nothing until
   the caller names a mode. Both `build` and `check` also accept a
   directory, in either of two shapes:
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

   `check` walks either shape exactly as `build` does and compiles every
   entry point it finds, differing only in that it writes nothing—so it
   takes no `--out` (there is no output to place, including in flat
   mode, where `build` requires one) and no `--force`.

   `build` prefixes every document it emits, on any of those paths,
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

The fourth construct of this shape is *not* a warning: a `router` block
that sets no `host` is a hard error—#144, redirected by #198 from "no
`expose.host`" to "no `router`" once `router` became the only source of
a Traefik router. The block that exists only to *be* a router says
nothing about which requests reach it, so no reading of it means
anything, while dropping it quietly shipped a service with its
forward-auth missing and nothing to say so.

Through #220 this check had a second shape, told apart by a `field`
discriminant on the same `CodegenError` variant: a service-level
`middleware` with no `router` anywhere to attach it to. #221 moved
`middleware` inside `router`, which makes that shape unwritable—the list
only exists within the block it attaches to—so the variant collapsed to
the one question it still answers and lost the discriminant with it
(`RouterWithoutHost`). What used to be a router-less `middleware` is now
either a host-less `router`, caught here, or the old spelling on a
service body, caught one stage earlier by `ParseError::MovedField`.

`traefik`'s own `disabled` flag raises the mirror-image error—see
#159: declaring a `router` block on a service that also switches Traefik
off. Through #197 this list also named `expose.host`/
`expose.entrypoint`—#198 removed both once `expose` stopped carrying
either field, and #221 removed the service-level `middleware` from it
the same way, leaving a `router` block the only construct to check. Both
diagnostics share one shape, something whose only
meaning depends on a router existing, contradicted by something else
the same service says about that very router, so this gets the same
hard-error treatment rather than a fourth warning. The router-less case
is a missing piece silently completing itself the wrong way. This one is
a direct contradiction between two things the author wrote on purpose,
which reads as even less likely to be an accident, not more. Letting the
flag lose silently would keep a router alive against the service's own
stated intent. Letting the router-attached field lose silently would
build a compile-broken deploy that looks fine until Traefik never picks
it up, and neither failure mode is one `hllc` should choose on the
author's behalf. `expose`'s own `port` is exempt: it's Compose's own
`expose:` key, plain container-network visibility with nothing to do
with Traefik, so a service with Traefik off may still declare one.

`router` adds hard errors of its own, all of the same
shape—something that can only be a Traefik router, either contradicted,
or left incomplete—#184, #198:

- A `router` block with no `host` has no rule to emit, so there is
  nothing it could have meant—this is `RouterWithoutHost`, described
  in the preceding paragraph. Through #197 this was stricter than
  `expose`, which tolerated a host-less block because its `port` still
  did a second job, Compose's own `expose:` key, that had nothing to do
  with Traefik. #198 removed the comparison entirely by removing
  `expose`'s own host-carrying router—today `router` is the only thing
  that can be host-less this way, and it always has no second job.
- A service with at least one `router` block but no `expose`-set `port`
  is `RouterWithoutPort`, new at #198: once `router` is the only source
  of a Traefik router and `expose` the only source of a port, "does this
  service have a router" and "does this service have a port" become two
  independent, directly checkable questions, closing a live defect the
  old coupled design carried—a service routed only by `router` blocks
  that forgot `expose <port>` used to emit no
  `loadbalancer.server.port` label at all, silently, leaving Traefik to
  guess one. It's the router-side mirror of `RouterBlockWithoutHost`: one
  variant catches a router with nothing to route *to*, the other a
  router with nothing to *balance onto*.
- A router name outside `[A-Za-z0-9_-]` draws a rejection—this is
  `UnsafeRouterName`. This is a different check from the metacharacter
  guard that covers label *values*, and deliberately a different
  character set: the name goes into the label **key**,
  `traefik.http.routers.<name>.rule`, where a `.` extends the dotted key
  and an `=` ends it outright, since Docker splits a label string on its
  first `=`. A bad name doesn't corrupt one label's meaning, it writes a
  different label. The grammar already refuses such a name—a router
  name is an `IDENT`—and codegen checks it again anyway, so its safety
  doesn't rest on the grammar staying as it stands.

The parser catches two `router` blocks in one body claiming the same
router id instead—`ParseError::DuplicateRouterName`—alongside the
other duplicate-key errors, since both spans are still in hand there.
Through #197 this covered only the hand-written case, two explicit
`router { }` blocks whether named or unnamed. A service that instead wrote both an
unnamed `router { }` and `expose <port> as "<host>"` reached a
separate, codegen-level check instead, `ExposeHostWithUnnamedRouter`,
since only codegen could see both the field-shaped `expose.host` and the
block-shaped `router` at once. #198 collapses that distinction: the `as`
sugar now desugars to an ordinary unnamed `router { host }` entry
*during parsing*, per the preceding "Secondary-field bare shorthand"
rule, so it collides with a hand-written unnamed `router { }` the same
way any other duplicate name would. #198 removes
`ExposeHostWithUnnamedRouter` rather than porting it forward—there's
nothing left for it to catch that `DuplicateRouterName` doesn't already.

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
