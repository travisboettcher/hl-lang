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

Reserved (not usable as IDENT): none
Punctuation: { } [ ] ( ) : = -> , . $
```

- **The language has no reserved words.** Every word that looks like a
  keyword (`template`, `service`, `network`, `image`, `build`,
  `volume`,
  `publish`, `env`, `env_file`, `restart`, `expose`, `healthcheck`,
  `depends_on`, `networks`, `dns`, `devices`,
  `container_name`, `command`, `entrypoint`, `privileged`, `with`, `as`,
  `external`, `use`, `raw`, `defaults`, and more) is an ordinary
  `IDENT`. The parser resolves it at parse time, against a schema table
  or by grammar position—never at the lexer level. They're all *contextual*
  keywords, meaningful only in the grammar position expected (the same
  technique as C#'s `var`/`async`/`await`/`yield`), not globally
  off-limits as identifiers.
- `template` was the one exception through #258, with a token kind of
  its own. It bought nothing the other contextual keywords don't already
  demonstrate: `template` leads a `template_decl` and `use` leads a
  `use_decl`, one token separates either from a `named_decl`'s type
  name, and `template` isn't a registered top-level type, so there's
  no competing parse to resolve. Reserving it only cost the positions
  where the word is just a name—`service template { ... }`, a parameter
  called `template`—and left the lexical grammar with one exception to
  explain.
- `.` separates two names, and which two depends on where it's written:
  an import alias from the name it qualifies (`alias.name`, see Imports,
  below) in a reference position, and a declaration from the field being
  read off it (`proxy.name`, see Syntactic grammar, below) in a value
  position. It appears nowhere else—`NUMBER` is integer-only, so there's
  no decimal-point ambiguity to resolve.
- `$` prefixes a reference to a `template`'s own declared parameter
  (`$port`), see Composition, below. It's reserved for exactly that one
  purpose—not a general sigil for anything else. It's a *token*, so it
  substitutes a whole `literal` and never reaches inside a `STRING`'s
  content: ``"Host(`$host`)"`` carries the five characters `$host` like
  any others, and reaches generated output as written. `{{param}}`,
  further down this list, is how a parameter gets inside a string.
  Composition warns when a template's own string holds a `$` naming one
  of that template's parameters, since the two spellings are easy to
  confuse and the wrong one fails silently—see
  `ComposeWarning::InertParameterInString`. A `$` naming anything else
  passes through untouched, because `command`/`env` carry `$HOME` to a
  shell and Compose reads its own `${VAR}` form out of the generated
  file.
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
- `{{binding}}`, string interpolation, is *not* part of the lexical
  grammar—it's ordinary content inside a `STRING` token that a later
  stage resolves. Two stages do, because each answers what the other
  can't see. Composition handles a binding naming one of the enclosing
  template's own declared parameters, since an invocation's bound
  arguments outlive nothing beyond that invocation's own resolution.
  Codegen handles `{{name}}`, the implicit binding to the enclosing
  service's own name, since only codegen knows which service a
  template's contribution finally landed on. Composition therefore
  hands `{{name}}` on untouched, along with any binding it has no
  parameter for, so a typo still reports as an unknown interpolation
  rather than reaching the output. Both stages find a binding with the
  same scanner, `hl_parser::interp::resolve_with`, rather than a loop
  each, so they can't disagree about where one starts and ends.
- `name` belongs to that service-name binding: a template *may* declare
  a parameter called `name`, and `$name` reaches it, but `{{name}}` goes
  on meaning the service. Deciding it that way round is what keeps
  parameter interpolation additive—every `{{name}}` written before
  parameters could interpolate at all still resolves to what it resolved
  to then—and a template that declares the parameter *and* interpolates
  the binding gets `ComposeWarning::NameParameterNotInterpolated`
  instead of a silent change of meaning.
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

value          ::= literal | list | statement

list           ::= "[" ( value ( "," value )* )? "]"

literal        ::= STRING | NUMBER | IDENT | IDENT "." IDENT | "$" IDENT
                  | field_access

field_access   ::= IDENT "." IDENT              # local declaration, field
                  | IDENT "." IDENT "." IDENT   # alias, declaration, field
                  | "$" IDENT "." IDENT         # bound declaration, field
```

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
  `env_file`, a `depends_on` entry's own reference) rejects
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
- **A list argument joins into text and splices into a list**, settled
  at #283 as the two things a list can honestly mean in those two
  positions. `{{items}}` renders the items separated by
  commas, so a template takes `["auth@file", "compress@file"]` and
  writes the one comma-joined label the built-in `middleware [...]`
  field generates. In a reference list, `networks $nets` puts the items
  where the parameter stood—as does `networks [$nets]`, which parses to
  the same one-element vector, and `[a, $nets, c]`, which splices in
  place. `dns`, `env_file` and `depends_on` behave the same, the guide
  grouping all four together for the reason they share here, and a
  `depends_on` entry carries a condition as well as a name—so each item
  spliced through that entry takes the condition written on it, the only
  reading that keeps what the author wrote.
  The comma is the whole join rule: naming a separator would be
  interpolation syntax to design for a caller that hasn't appeared, and
  a template needing another one takes the joined string instead.
  An empty list means what an empty list means in each position—no
  characters when joined, no elements when spliced—rather than drawing
  an error, since the reason to refuse it would be a judgement about
  what a downstream consumer does with an empty value.
  Both positions refuse nesting, and this is the load-bearing part: `[a,
  [b]]` and `[a, b]` are different values, so flattening them together
  would be exactly the silent coercion `TemplateArgumentNotScalar`
  exists to refuse. A spliced item also faces the reference-shape check
  a written element faces, since arriving inside a list doesn't make a
  bare number legal where the grammar never allowed one. Both refusals
  name the *item*, which is what the author has to change.
  A single-value slot still takes no list at all: `container_name $xs`
  is `TemplateArgumentNotScalar` however many items `xs` holds, because
  there is no honest way to put several values where one belongs.
- A parameter reaches a value two ways, and they aren't
  interchangeable. `$param` substitutes a whole `literal` slot, span
  included, so the argument's own literal kind is what lands in the
  field—which is what the preceding reference-shape and `number` checks
  examine. `{{param}}` substitutes *text* into a `STRING`'s content, so
  the argument contributes its characters and the slot stays a string: a
  template can build ``Host(`media.example.com`)`` out of a `host`
  argument rather than taking the whole rendered rule. Composition
  rejects an argument with no text form there—a nested map—with
  `ComposeError::ArgumentNotInterpolable`, though it remains a
  perfectly good whole-slot argument elsewhere.
  A parameter forwarded into a nested invocation (`with inner { h:
  $host }`) has nothing concrete to splice yet, so the interpolation is
  *renamed* into the enclosing template's parameter namespace—`inner`'s
  `{{h}}` becomes `{{host}}`—which is the scope the string now lives in,
  and whichever call site finally binds `host` resolves it. That mirrors
  what the whole-slot form already does, where forwarding replaces one
  `Literal::Param` with another.
  A third form reads a field off whatever the parameter names rather
  than substituting the parameter itself: `$net.name` as a whole value,
  `{{net.name}}` inside string content. Both defer the same way a
  forwarded parameter does—`{{net.name}}` becomes `{{proxy.name}}` once
  an argument binds `net`, leaving a dotted binding for the pass that
  resolves those—so one parameter serves both the `networks [$net]`
  entry that wants the identifier and the label value that wants the
  real Docker name. An argument with no declaration to name (a number, a
  list, a nested map) draws an error at its own call site, the way the
  reference-shape and `number` checks report theirs.
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
  in, `networks`/`dns`/`env_file`/an `entrypoints` or `middleware`
  list/a
  `depends_on` entry included—a template can write `networks [$net]` or
  `labels { "k": $v }` exactly as freely as `restart $policy`, since there is
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
- `field_access` reads one field off a `network` or `volume`
  declaration, and it's legal **in a value position only**—a `labels`
  entry's value, an `env` value, a `raw` value, a `with`-invocation's
  argument, and every other slot the `value` production reaches. A
  declaration has two names, the identifier `.hll` refers to it by and
  the real Docker name (`name:` when set, the identifier otherwise), and
  reading the second is what the feature exists for.
- **A field is readable when it holds a value**, which is a property of
  the field rather than a list this language keeps. `name` holds one on
  both kinds, and so does a `volume`'s `driver`. A bare-presence flag
  such as `external` and a nested map such as `driver_opts` hold nothing
  a value position could take, so each says so and names itself: a field
  written three lines up is real, and refusing it as "no such field"
  sends the reader hunting a typo that isn't there. A field the kind
  genuinely hasn't got draws the other error, which lists what the kind
  does expose—read off the kind itself, so it can't go stale the next
  time a declaration grows a field. A field the kind has that this
  declaration leaves unset (a `volume` with no `driver`) draws an error
  rather than reading as the empty string. Docker picks the default in
  that case, and no honest text spells the default it picks.
- **A reference-shaped position takes no field access, on purpose**,
  which is what keeps the grammar unambiguous. `.` in a reference already means
  `alias.name`, so `networks [proxy.name]` can only go on meaning the
  network `name` that the file aliased `proxy` exports—and
  `networks [$net]` has to go on taking the identifier, since attaching
  a network is what that list does. The two spellings a reference
  position *can* tell apart, a third segment and a `$param` base, draw a
  diagnostic saying which position field access belongs in, rather than
  a stray "expected `,`" a token later.
- **Inside a value position, the segment count decides the shape.** Two
  segments name a local declaration and a field, three name an alias, a
  declaration and a field, and a `$param` base takes exactly one field,
  since a parameter already names one declaration. A fourth segment has
  no shape left to be, so it draws an error rather than a partial
  reading of the first three.
- **A `with`-invocation's argument is the one value position where two
  segments may instead name an imported declaration** (`with
  docker_network { net: shared.proxy }`), because a parameter is the one
  value that goes on to be a *reference*: bound to a declaration, it
  reaches `networks [$net]` and `{{net.name}}` in the template it was
  passed to, exactly as a bare same-file `IDENT` argument already does,
  which is how one shared `network.hll` serves the template files that
  label it. Precedence settles the ambiguity rather than syntax: a base
  naming any of the program's own declarations keeps the field-access
  reading it has always had, and only a base naming none of them takes
  the alias reading, so no access already written can change meaning. A
  base naming neither still reports as the field access the parser made
  of it. A base naming a real alias that holds no such declaration says
  *that* instead, since "no such local declaration" would name the wrong
  mistake.
- **A declaration bound to a parameter is a reference, not a value.**
  The template that takes it may attach it (`networks [$net]`) or read a
  field off it (`$net.name`, `{{net.name}}`). Splicing it into an
  ordinary value draws an error at the argument, since a declaration has
  no text form an `env` value or an `image` could take. Attaching one
  imports it, on the same terms a written `networks [alias.name]` does,
  bare-name collision check included, while reading a field still
  imports nothing.
- The same access interpolates into string content as a dotted
  `{{binding}}`—`"prefix-{{proxy.name}}"`, `"{{alias.proxy.name}}"`,
  `"{{net.name}}"` for a parameter—reading exactly as it does in source,
  segment count included. `{{name}}`, the enclosing service's own name,
  and every other binding without a dot keep their meaning: a dotted
  binding is new syntax that never collided with them.
- **Composition resolves every field access**, into a plain string
  holding the declaration's real name, so codegen never learns the
  syntax exists. Two passes do it, split by what each stage can answer.
  A scope's own body resolves first, in the scope that wrote it, which
  is the only place an import alias means anything: the lexical-scoping
  rule in the following Imports section says a template's
  `traefik.proxy.name` resolves against the file holding that template,
  and after the invocation resolves, that file is no longer in hand.
  Whatever is still a `$param` at that point resolves in a second pass
  over each service's fully merged fields, once every argument has a
  binding. Between the two sits a third, narrower pass, over one
  invocation's substituted body: an argument naming an imported
  declaration leaves an `alias.decl.field` access behind in the template
  it reaches, and the alias means something only in the file that
  wrote the argument. That file is the invocation's own scope, in hand
  there and gone by the time the merged fields reach a service. Reading
  a name attaches
  nothing: `networks [...]` is what attaches a network, so a label
  naming one leaves the generated `networks:` section alone.

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
   schema drives one further generic continuation: additional explicit
   `key: value`/`key` fields may follow, each preceded by a **mandatory
   comma** (the same "trailing comma continues, its absence ends the
   statement" rule as any other comma-list): `build "./app", dockerfile:
   "Dockerfile.prod"`. A field whose own value is an unbracketed
   comma-list ends at the next `key:` rather than swallowing it, by the
   same one-token lookahead.
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

   `expose <port> as "<host>"` was a second, bespoke fusion onto the
   primary value with no comma, desugaring to `expose { port }` plus an
   unnamed `router { host }`. It went with routing at #271: there is no
   `router` node left to desugar into, and a port and a hostname were
   only ever fused because one built-in owned both. `as` is an ordinary
   identifier again.

4. **Repeatable-field accumulation**—semantic, not part of the
   Context-Free Grammar (CFG)—writing `volume`, `publish`, `env`,
   `depends_on`, or `labels` more than once in
   one body appends, since those fields are list/map-kinded—subject to
   the set-like lists' distinct-name rule under "Composition" below, which
   drops a repeat of a name already present (`depends_on` instead keeps
   only its own list's *last* entry for a repeated name, per the same
   keyed-merge rule its own paragraph under "Composition" describes—#155).
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
| `volume`—the `service`/`template` field | map |—| `->` | value—the container path | no |
| `driver_opts`—inside a `volume` declaration | map |—| `:` | key | no |
| `publish` | map |—| `->` | value—the container port | no |
| `devices` | map |—| `->` | value—the container device path | no |
| `env` | map |—| `=` | key | no |
| `labels` | map |—| `:` | key | no |
| `restart` | struct | `policy` |—|—| no |
| `healthcheck` | struct |—|—|—| no |
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
`env`, `labels`, `driver_opts`, and `raw` are map-kind too but outside
this group—their uniqueness lands on the key side instead, per the
preceding table, so they never shared `volume`/`publish`/`devices`' own
value-side convention to begin with.

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
composing two templates could lose a value in silence.

#193 was a *composition*-level statement, and #206 completed it with the
parser-level one it implied. The parser checks a `raw` body's own entries
against each other now, so a key repeated within one body—one `raw { }`
block or two in the same `service`/`template`—raises the same
`DuplicateMapKey` a repeated `env` key in one body always did, naming
both spans. `raw` is no longer the one map field where a repeat inside a
single body quietly loses a value.

That check covers one mapping and nothing outside it, which is the only
scoping `raw` admits: it's schema-free, and its values recurse, so a
`raw` body holds a tree of mappings rather than one flat list of keys.
The rule follows YAML's own—a duplicate key belongs to a single mapping
and says nothing about any other—so the parser checks each nested map on
its own, against the keys in that one mapping and nothing else.
`raw { a: { x: 1 }, b: { x: 2 } }` holds two mappings that each happen to
carry an `x`, so it compiles. So does a nested map that reuses a key its
enclosing mapping already claims. Only a mapping that names one key twice
draws an error.

Codegen has a separate rule about `raw` keys, unaffected by #193, because
YAML forces the issue—two keys spelled the same in one mapping is
invalid—and a `raw` key may well name a field that has a preceding row.
Where it does, codegen emits the `raw` value and drops the built-in one,
which keeps adding a row to this table from breaking files that were
already reaching for `raw` in that row's absence (see the `raw` section
of the book's Built-in Fields page).

`labels` is the one key where that rule needs saying out loud, and #232
is why. Every other overridable key is one built-in field, so replacing
it means replacing the one thing the author wrote. `labels` is an
*aggregate* instead: its entries reach a service from every template
tier applied to it as well as from its own body, and since #271 that
includes all of the service's routing.

A service applies a template without knowing what else contributes to
the key. So `raw { labels: [...] }` doesn't override *a* field. It
discards every contributor's output at once, and overriding it commits
the author to reproducing all of it by hand and keeping it in step from
then on. That's still the correct behavior—a half-merged `raw` value
would no longer be verbatim passthrough, and hand-writing the whole list
is a legitimate use of the escape hatch—but losing the whole set in
silence is a trap worth a diagnostic, so codegen raises a
`RawLabelsReplaceGenerated` warning whenever a service has labels and
`raw` names the key too. A warning rather than an error: no file stops
compiling, and no generated document changes.

Codegen raises that warning only where the computed label set comes out
non-empty, so it fires exactly when the override actually costs
something. A service with no labels of its own and none from a template
generates none for `raw` to replace, and says nothing.

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
`.`: a wrong default here builds the wrong directory rather than
nothing.

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
own: `host` generated the router-rule label and `entrypoint` restricted
it to named entry points. #198 moved both onto `router`, and #271 moved
routing out of the language entirely, leaving `expose` with the one
field it still has, `port`, doing the one job Compose gives it.

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

Routing was a built-in through #270, and its removal at #271 is the
largest single subtraction this language has made. It's recorded here
rather than deleted, because the reasoning generalizes.

`router` was the table's one row that was both repeatable *and*
struct-kind, keyed by a name the user wrote. Around it sat `traefik {
disable }`, a `rule` sub-grammar with a table of Traefik's own matchers,
`middleware` and `path_prefix` and `entrypoints` lists, an `expose
<port> as "<host>"` sugar, and ~2,570 lines of codegen assembling label
strings. All of it modelled one reverse proxy.

That fails this document's own opening principle: *would this make sense
on a homelab with completely different infrastructure?* A `router` block
means nothing without Traefik, and the compiler had no business knowing
what a rule was. So `hllc` now generates no routing labels at all. A
service's routing is `labels` entries, and templates write those
entries—`std:traefik` for the common case, covered under "Modules
bundled with the compiler," or your own for anything else.

What the removal cost, stated plainly: `hllc` used to parse rule syntax
and reject an unknown matcher, a wrong argument count, or a backtick in
a host that would close a `Host(` call early. It can't any more, because
a label value is a string. The alternative was a validator tracking a
third party's syntax across its releases, inside a compiler whose whole
claim is that it doesn't know about that third party. The check moved to
Traefik's own startup, which is where someone maintains it.

What the removal required is the more interesting half, because none of
it was foreseen from the outside. Six capabilities the generic core
lacked had to be *added* before the specific feature could be *deleted*:
#266 interpolated a parameter into string content. #267 gave the
compiler a way to ship modules at all. #275 read a declaration's real
Docker name from a value, which `traefik.docker.network` needed and no
built-in had ever exposed. #283 passed a list to a template. #288 made a
list-valued `labels` entry concatenate across tiers rather than collide.
The vendor integration had been standing
in for all of them, which is why the gaps were invisible while it
existed. Every one is now available to any template, for infrastructure
this compiler has never heard of.

Two smaller things went with it. `AmbiguousExternalNetwork` refused a
service with two external networks purely because the compiler had to
pick one to name in `traefik.docker.network`. Once a template writes
that label and takes the network as an argument, there is nothing left
to disambiguate. And `CodegenError::LabelCollidesWithGenerated` reduced
to `DuplicateLabelKey`: with no generated labels to collide with, the
only remaining collision is between two entries the author wrote.

The old spellings don't vanish silently. `router`, `traefik`,
`middleware` and `expose <port> as "<host>"` are all
`ParseError::MovedField` (`schema::moved_field`), each naming the
`std:traefik` template that replaces it, rather than the generic
`UnknownField`—whose advice on these types is the `raw { ... }` escape
hatch, which here would compile and emit a meaningless `router:` Compose
key while the routing quietly went missing. That's the same "valid
output, wrong service" failure #144 closed off, arrived at through a
helpful hint.


#243 added `labels` as the *additive* label form, and since #271 it's
the only one: every label a service carries is a `labels` entry,
whether written in its own body or contributed by a template it applies.
Entries land in tier order—each `with` target left to right, then the
body—so a service that applies no template emits exactly what it writes.

Before #243 there was no additive form at all. The only way to write one
extra label line was `raw { labels: [...] }`, which replaces the whole
set—so a service wanting one extra line silently lost every other label
it had, and had to reproduce them by hand and keep them in sync. That's
#232, reported against a real conversion, and #231 is the concrete need
behind it: per-router TLS Subject Alternative Name (SAN) domains, which
the `router` field of the day had no home for. A first-class additive
`labels` covers it without a schema row of its own.

The row is map-kind rather than a list of `"key=value"` strings, and the
choice is the same one #193 and #206 settled elsewhere. A list would read
closer to Traefik's own documentation and would need no quoting around a
dotted key. It would also be the one collection in the language with no
uniqueness side, so a key written twice would silently keep the last
value—exactly the hazard those two issues closed for `raw`, reintroduced
in a brand-new field. A map reuses `TypeSchema::uniqueness` as it stands,
so the parser names both spans. Compose's own `labels:` key takes a map
form natively as well, so this is also the spelling closest to Compose.
The cost is real and worth naming: a key such as
`"traefik.http.routers.web.tls.domains[0].main"` needs quotes around it,
because dots and brackets can't appear in a bare word.

Two `labels` entries resolving to one key is a hard error naming both
sides, not a precedence rule. The parser catches the ones spelled
identically, and codegen catches the rest: `{{name}}` resolves by then,
and two keys spelled differently in source can land on one.
Neither precedence is available: whichever entry lost would be a line
the author wrote that quietly does nothing, which is the failure #144,
#193, #206 and #232 all exist to close. Refusing is the only outcome
that keeps the language's promise that a line either takes effect or
draws a diagnostic. Because the check reads the *key*, a key holding an
`=` would defeat it—Docker splits a label at its first `=`, so
`"a.b=x"` would emit a label the check never saw. Codegen rejects an `=`
in a key for that reason.

`raw { labels: ... }` keeps its documented full-override semantics
unchanged, and overrides a `labels` field too: `raw` replaces the emitted
Compose key, not any one contributor to it, so the escape hatch stays
exactly as blunt—and exactly as predictable—as it has always been.

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

Through #198, the identifier `entrypoint` named two unrelated things,
the way `volume` still does: this service-level command override and a
router's own list of Traefik entry-point names. The grammar was never
ambiguous—the parser resolves a field name only through
`schema::resolve_field` against the enclosing type's own field list, so
the two tables were never consulted in each other's position—but a
reader had only position to tell them apart, one line from the next in
the same service body. #199 renamed the router's field to
`entrypoints`, and #271 removed it from the language altogether, so
`entrypoint` names one thing again. `volume`'s own pair stays
untouched: a `volume` declaration and a `volume` mount are two distinct
Docker concepts that Docker itself spells the same way, so renaming
either would move `hll` further from the thing it models rather than
closer.

`privileged` isn't a row either, for the same
reason `NETWORK`'s `external` isn't: a bare-presence `FieldKind::BoolFlag`
directly on `service`/`template`, matching Compose's own `privileged:`
key—see #157. `template` isn't a
row either—it's the mechanism for adding new rows to this table at
parse time. Neither is `defaults`, which through #260 named a template
the compiler applied on its own and is now just a template name like
any other—see Composition, below.

## Composition: templates and `with`

A `template` is a named, optionally parameterized block that produces a
*partial* record of fields for `with` to merge onto a real `service`.
Templates must be fully applied at each call: never partially applied,
and never curried. A template's body can itself `with` other
templates—composition.

A template applies to a service only where that service's own `with`
names it. No template applies on its own, and no template name means
anything special to the compiler.

Merge priority, lowest to highest:

1. `with`-listed templates, left to right—a collision between two of
   these on the same scalar/map field is a **compile error**
2. the service's own body—always wins over everything

#260 removed a third tier below both: a template named exactly
`defaults`, implicitly applied to every service in its file, which never
took part in conflict checking and so always silently lost. It couldn't
cross a `use` boundary, because having no invocation left no alias for a
lookup to go through—so the case that would have paid for it, sharing
one baseline across files, was the one case it structurally couldn't
serve. Naming the template and applying it with `with` costs one line
per service, works across files, and leaves the merge rules with one
fewer tier to restate. A `defaults` template nothing invokes now warns,
per the Diagnostics section, since the old behavior would otherwise stop
applying in silence.

List fields concatenate, so no collision is possible. The set-like ones
(`networks` alone) concatenate by *distinct* name, keeping the first occurrence, while `dns` and
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
paragraph—which happens to produce the same "own wins, two explicit
collide" result for the common case of the same tier
repeating the same mapping, but now raises a genuine `MapKeyCollision`,
the same one `publish` would, when two *explicit* templates map
different hosts onto the same container path. `privileged` gets the
same collision rule as a scalar
field even though it isn't one—see the `healthcheck.test`/`.disable`
paragraph below for how a bare-presence flag rides the same
Own-always-wins/two-explicit-collide rule
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
that each depend on the same service: naming one thing more than once
isn't an ambiguity between it and itself, it's one answer given twice.

`labels` merges by key like `env`, with one rule of its own, settled at
#288: a **list-valued** entry concatenates across tiers instead of
colliding, dropping a repeat of a value it already holds, while a
**single-valued** one keeps `env`'s rules exactly—the service's own body
wins over a template, and two explicit templates collide. The shapes
mean different things about merging, and that's what the distinction is
for. A single value says the key holds one thing, so two templates
setting it are two answers to a one-answer question, which is the
collision #243 exists to raise. A list says the key holds several, so
several places contributing is the whole idea. Generic rather than a
carve-out: the rule is about lists, and it's the same reasoning
`networks`, `dns` and `entrypoints` already merge by. It exists because
#271 took `router.middleware` out of the compiler, and with it the only
way two independent templates could each contribute one middleware to a
service—a composition pattern this repo's own fixtures teach, and one a
checked map can't express without it. One key written as a list in one
place and a single value in another is
`ComposeError::LabelShapeMismatch`: they disagree about what the key
holds, and either resolution silently discards what the other said.

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
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{name}}.internal.techdebtor.io`)"
    "traefik.http.routers.{{name}}.entrypoints": "web-secure"
  }
}

service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just the rule—the port and the entrypoints label still
  # come from internal_web above
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`tools.internal.techdebtor.io`)"
  }
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
- **A path starting `std:` names a module bundled into the compiler**
  rather than a file, and never reaches that relative resolution at
  all—see the following "Modules bundled with the compiler."
- `alias.name` qualifies any reference: a `networks [...]` entry
  (`networks [traefik.traefik-net]`), a named-volume mount's host side
  (`volume storage.media -> "/data"`), a `with` invocation's target
  (`with common.internal_web { ... }`)—and, syntactically, every other
  reference-shaped position too (`dns`, `env_file`, `depends_on`),
  since `alias.`
  and `$param` are the same `literal` production wherever it's
  written—see the preceding Syntactic grammar section. Only `networks` and a
  named-volume host actually *resolve* one, though: they're the two
  positions with a real cross-file declaration to resolve a qualifier
  against. Every other reference-shaped position rejects one outright,
  with `UnsupportedQualifiedReference`, exactly as it always has—none of
  them has a coherent cross-file meaning: `depends_on` names a same-file
  sibling service, and `dns`/`env_file` aren't resolved against
  anything an `.hll` file declares at all—an `env_file` entry names a path on disk next to the
  generated Compose file. `devices` was never a candidate for a
  qualified form in the first place—#167 made its entries plain
  literals, like `publish`'s and `env`'s, neither of which is
  reference-shaped either.
- `alias.name.name` reads a field off an imported declaration, in a
  value position, per the preceding Syntactic grammar section's
  `field_access`. Unlike a qualified *reference*, it pulls nothing
  across the import: it resolves to the declaration's real Docker name,
  a string, so the importing program's own `networks:`/`volumes:`
  sections stay as they were. That also means it never trips the "an
  imported network keeps its own bare name" collision below, since no
  second declaration comes over.
- `alias.name` in a `with`-invocation's argument is a qualified
  *reference*, not a field access, and behaves like every other one: a
  template attaching it to `networks` imports the declaration and can
  collide on its bare name, while a template that only reads a field off
  it imports nothing. The precedence rule in the preceding Syntactic
  grammar section decides which of the two a two-segment argument
  is—a local declaration first, an alias only otherwise.
- **Templates are lexically scoped, not dynamically scoped.** If a
  template declared in `templates.hll` writes
  `networks [traefik.traefik-net]`, that `traefik` resolves against
  *`templates.hll`'s own* `use` declarations—never whichever file
  happens to invoke the template with `with`. A template's references
  always resolve relative to where it was *written*, not where it was
  *called from*. A field access follows the same rule for the same
  reason, including one written in a `with`-invocation's arguments,
  which is a value the calling file wrote and therefore resolves in the
  calling file's own scope.
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
  it, since nothing resolves a service across files anyway. That's a
  warning rather than an error—see the following Diagnostics section.
  `use` does share every template, `defaults` included: since #260
  naming one in a `with` is the only way any template reaches a
  service, so
  `with common.defaults` resolves exactly as any other qualified
  invocation does.

### Modules bundled with the compiler

`hllc` carries `.hll` modules inside its own binary, and a `use` path
prefixed `std:` names one of them:

```
use "std:traefik" as traefik
```

The namespace carries one module, `std:traefik`, added at #269. #267
landed the mechanism a release ahead of it because that module has to
reproduce the labels this compiler generates, byte for byte, which makes
shipping alongside the compiler the whole point of it. A copy in the
user's own tree drifts from the compiler that generated it the moment
either one moves, and drift in a template whose job is byte-for-byte
agreement means silently wrong output rather than a diagnostic.

**`std:traefik` is one template per label**, rather than one `router`
template taking every optional field, and the reason is a property of
the language rather than a preference. A `labels` block writes every key
it lists, and nothing omits one, so a single template would emit
`entrypoints=` for a router that has no entrypoints. Each label a router
may or may not carry gets its own template, a caller lists the ones it
wants, and the `with`-list order is the label order. A composite covers
the common shape over those primitives. The HTTP and TCP sets are
separate for the same kind of reason: the namespace is part of the label
*key*, and no template picks a key by condition.

`traefik.docker.network` is a template here like everything else, taking
the network as an argument and reading its real Docker name through
`{{net.name}}`, per #275. The compiler derived it through #270, and #271
moved it in the same change that removed the derivation, since a template
writing the label while the compiler still derived it would collide with
it—there was no intermediate state where both spellings worked.

**A rule handed to a template is a string.** Nothing parses it. A
misspelled matcher compiles and fails at Traefik as a router that never
matches. That validation was Traefik-shaped knowledge, and losing it
costs exactly what the compiler saves by carrying none—the same trade
#270 settled for label values.

**Why a prefix rather than new syntax.** The obvious alternative,
`use <std/traefik>`, costs two tokens the lexer has never carried—`<`
and `>` appear nowhere else in the language—plus a second `use_decl`
production to reach them. The prefix costs neither: `use_decl ::= "use"
STRING "as" IDENT` covers it exactly as written, so the preceding
Lexical grammar and Syntactic grammar sections need no new exception,
and the whole of the feature lives in the linker, the stage that already
decides what a path means.

**The compiler reserves the `std:` prefix.** A path that starts with it
names a bundled module and never reaches relative resolution, so no file
answers to one—whatever a user names a file, and however some other
`use` spells its path. Stated plainly, the consequence: a file literally
named `std:traefik.hll` no longer answers to `use "std:traefik.hll"`.
Reaching that file takes an explicit relative spelling,
`use "./std:traefik.hll"`, and even then a diagnostic can't tell the two
apart by name, since a bundled module renders under exactly the same
`std:traefik.hll`. That's an accepted trade rather than an oversight: a
`:` in a filename is rare enough to make the collision a curiosity,
while a namespace any file can shadow by name defeats the point of
shipping a module with the compiler at all.

**A relative `use` inside a bundled module stays inside the standard
library.** A compiled-in module has no directory in the user's tree to
be relative to, so a `use "labels.hll"` written inside `std:traefik`
names `std:labels`, and `use "std:labels"` names that same one module
rather than a second copy of it. Every other rule holds unchanged: an
absolute path, or one climbing out through `..`, draws the same
rejection there that it draws in a user's own file. Cycles need no new
machinery either—a bundled module joins the same module graph under an
identity of its own, so the load-once-per-module rule that already keeps
`A` uses `B` uses `A` from looping covers a cycle through the standard
library too.

**What this namespace never grows into.** No search path, no `$HLL_PATH`
or `~/.hllc/lib`, nothing fetched over a network, and no version inside
a module path, such as `std:traefik@2`. One namespace, one copy of the
bytes, shipping with the compiler that reads them. Ambient state is
precisely what the relative-only rule for ordinary paths keeps out of a
build, and a module path that can name two different bodies is the seam
a package manager grows from.

Bundling instead of versioning has its own consequence, and it belongs
here rather than in a footnote: the modules move with the compiler, so a
release can change what one of them generates, and nothing pins an older
copy. A change to a bundled module is a change to what a user's build
generates, and it takes the same semver label any other such change
takes—see CONTRIBUTING.md's "Picking a semver label."

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
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{name}}.internal.techdebtor.io`)"
    "traefik.http.routers.{{name}}.entrypoints": "web-secure"
    "traefik.http.routers.{{name}}.middlewares": ["local-ipwhitelist@file"]
  }
}

template authenticated {
  labels {
    "traefik.http.routers.{{name}}.middlewares": ["forwardAuth-authentik@file"]
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
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{name}}.internal.techdebtor.io`)"
    "traefik.http.routers.{{name}}.entrypoints": "web-secure"
    "traefik.http.routers.{{name}}.middlewares": ["local-ipwhitelist@file"]
  }
}

template authenticated {
  labels {
    "traefik.http.routers.{{name}}.middlewares": ["forwardAuth-authentik@file"]
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

1. **Lexer** (`crates/hl-lexer`)—no reserved words at all,
   string/number literals, `{`/`}`/`[`/`]`/`.`/`$`, `->`, `:`, `=`,
   `(`/`)`, `,`, and `#` line comments. Every word is just an
   identifier to the lexer. Meaning comes from the schema table, or from
   grammar position, during parsing.
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
   `with`-list into a fully merged `Service` with no templates,
   unresolved parameters, or unresolved field accesses left, per the
   Composition section's 3-tier merge
   rules. A `declaration.name` becomes the plain string holding that
   declaration's real Docker name here, resolved in the scope that wrote
   it, so codegen never learns the syntax exists. Generalized over a `SymbolResolver` trait so the same merge
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
   whatever it names explicitly. That attachment is unconditional: a
   service that already carries its own `networks` list gets `default`
   added to it, rather than the list suppressing the attachment. A
   single-service program gets no such attachment, since Compose's own
   default there is already implicit for free. An explicit `network
   default { ... }` declaration still wins over both of those: its
   `external`/`name` settings apply as they would to any other network,
   and it still emits its own `networks:` entry.

   That unconditional attachment is deliberately *not* Compose's own
   rule, and per #182 the difference is worth stating rather than
   glossing over. Compose hands a service the default network only when
   the service declares no `networks:` key at all. A service naming even
   one network joins that network and no other. `hll` attaches `default`
   either way, because of what `default` is for here: in a file holding
   a whole stack, it already supplies the connectivity between those
   services that a hand-declared private network would supply, for free,
   with nothing to declare. A file converted from hand-written Compose can
   therefore *drop* such a network instead of reproducing it, which
   makes the converted file simpler than its original rather than a
   workaround for one. The divergence isn't a fidelity gap to close: the
   generated document means exactly what Compose reads it to mean, and
   an unwanted `default` entry costs a service nothing beyond a line of
   YAML.
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
file, a `template defaults` no service applies with a `with`, and a
top-level `network` no service references. That last one drops out of assembling
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

A fourth construct of this shape used to be a hard error rather than a
warning: a `router` block that set no `host` had no rule to emit, so no
reading of it meant anything, per #144 and #198. #271 removed it with
the field, along with `RouterWithoutPort`, `UnsafeRouterName`, and the
`traefik { disable }` contradiction check. Every one of them asked a
question about a Traefik router, and `hllc` no longer knows what one is:
a set of `labels` entries that describes a router badly is a set of
labels, and the compiler has no basis to say otherwise. That's the
cost side of #271's trade, recorded in the preceding schema section.

`labels` carries the checks that survive, both #243's in origin—a
written string that would name a different label than the one written,
or a second answer to a question the service already answered:

- Two `labels` entries that resolve to one key after `{{name}}`
  interpolation is `DuplicateLabelKey`, naming both spans. The parser's
  own duplicate-key check can't see these: two keys spelled differently
  in source can resolve to one by the time codegen holds both as
  strings. Through #270 this variant was `LabelCollidesWithGenerated`
  and also covered a key one of the compiler's own features generated.
  #271 left it nothing to collide with but another written entry.
- A hand-written key containing `=` is `UnsafeLabelKey`. Docker splits a
  label at its first `=`, so such a key ends there and the rest joins
  the value—a forged label, and one the preceding check can't see, since
  the key it compares still holds the `=`. Control characters are
  refused with it, for the reason the label-value guard refuses them:
  #181's string escapes made a newline writable, and no label key holds
  one on purpose.

- A label **value** draws no check at all, settled at #270 as a decision
  rather than a gap. The old metacharacter guard on `router.host` worked
  because codegen knew the grammar that host landed in: a backtick there
  closes the `Host(` call early and widens the rule to match everything,
  which is #65. A `labels` value has no such
  grammar to know. A legitimate Traefik rule is mostly backticks,
  parentheses and `||`, so any guard strict enough to stop the dangerous
  string also refuses the ordinary one, and a guard tuned to Traefik's
  grammar is the Traefik coupling this design spent #259 removing. Two
  things make accepting that defensible. The text sits in the author's
  own `.hll` source rather than arriving from a stranger, so a bad value
  breaks the author's own homelab instead of opening it, and the same
  author already reaches for `raw` when they want codegen to stand
  aside. The duty this shifts—vetting anything spliced into a value with
  a syntax of its own, a template parameter most of all—belongs to
  whoever writes the template, and `book/src/` says so where it teaches
  both `labels` and interpolation.

A `labels` key repeated within one body is the parser's business
instead, as `ParseError::DuplicateMapKey`—the same error a repeated
`env`, `volume`, `publish` or `raw` key raises, from the same
schema-declared uniqueness side.

`ParseError::DuplicateRouterName` and the codegen-level
`ExposeHostWithUnnamedRouter` that preceded it both went with #271,
along with the name-keyed field shape they policed: no built-in field
takes a user-written name any more, so no two blocks can claim one id.

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
