# Language simplification audit

An audit of the `.hll` language and its implementation, looking for
constructs to **combine**, **generalize**, or **drop**—both to shrink
what a new user has to learn and to shrink the number of places a change
has to reach.

This document identifies and ranks. It changes nothing. Every proposal
preserves the supported feature set and the generated Compose output
unless its own "Compose output" line says otherwise.

Baseline: `v0.22.0`.

## Where the complexity sits

| Surface | Count |
|---|---|
| `FieldKind` variants, meaning parse plus merge strategies | 9 |
| `TypeSchema` statics | 17 |
| `service`/`template` fields | 22 |
| Bespoke `MergeAcc` slots outside the two generic tables | 6 |
| `ParseError` variants | 16 |
| `ComposeError` variants | 21 |
| `CodegenError` variants | 12 |
| `docs/DESIGN.md` | 1122 lines |
| `book/src/built-in-fields.md` | 1117 lines |

The schema-table architecture does its job: 22 service fields cost one
parse function, not 22. The complexity that has accumulated sits almost
entirely in the **value model**. The language carries several
near-identical value types, and every field kind that fits none of them
buys a dedicated merge slot. Nine of the last ten feature releases added
exactly one such exception. The findings below trace that.

## Findings

Ranked by payoff. Each states what's duplicated, what it costs, the
proposal, and the effect on generated Compose.

---

### F1—`expose` and `router` are one concept split in two

**Evidence.** `expose { port, host, entrypoint }` and `router <name> {
host, entrypoint, path_prefix }` share `host` and `entrypoint` with
*identical* semantics: the same metacharacter guard, the same `{{name}}`
interpolation, the same comma-joined `entrypoints=` label, and the same
router-rule shape. Yet they reach the output through two separate merge
paths—`SCALAR_FIELDS`/`LIST_FIELDS` rows for one, `MergeAcc::routers`
plus `merge_routers` plus `merge_router_host` for the other—and the
language carries four diagnostics whose only job is reconciling the
pair:

- `RouterFieldWithoutHost` and `RouterBlockWithoutHost`, which
  `hl-codegen`'s own doc comment calls "exact analogues, one level in"
- `ExposeHostWithUnnamedRouter`, which exists only because both spellings
  produce the label id `traefik.http.routers.<service>`
- `TraefikDisabledWithRouterField`, which has to enumerate
  `expose.host`, `expose.entrypoint`, and `middleware` by name

The book needs a whole **"Choosing between `expose` and `router`"**
section. A decision section reliably marks an unforced choice the
language pushes onto its users.

**A live defect this hides.** A service whose only routing comes from a
`router` block emits no `loadbalancer.server.port` label. Verified:

```hll
service a {
  image "nginx"
  router api { host: "x.example.com" }
}
```

That compiles clean and emits one rule label and no port, which leaves
Traefik to guess the backend port. The book says to "still write `expose
<port>`", but nothing enforces it. Splitting one concept in two is what
made the gap expressible.

**Proposal.** Give each half one job:

- `expose` keeps **only** `port`, meaning Compose's `expose:` key plus
  `loadbalancer.server.port`. It stops being a Traefik field.
- `router` owns **all** Traefik routing. The unnamed `router { }` form
  covers the common single-router case.
- Keep `expose <port> as "<host>"` as pure sugar that lowers to `expose
  { port }` plus an unnamed `router { host }`. The shortest spelling
  survives verbatim.

**Deletes.** The `expose.host` and `expose.entrypoint` schema rows, two
`SCALAR_FIELDS` rows, one `LIST_FIELDS` row,
`ExposeHostWithUnnamedRouter`, `RouterFieldWithoutHost`, which folds into
`RouterBlockWithoutHost`, `merge_router_host`'s divergence from
`merge_scalar`, `traefik_conflict_field`'s field enumeration, and the
book's decision section.

**Enables.** With one place a router comes from and one place a port
comes from, "a router with no port to balance onto" becomes a checkable
condition instead of silent output.

**Compose output.** Unchanged for every file that compiles today.

---

### F2—`Reference` and `Literal` should be one value type

**Evidence.** `FieldKind::ReferenceList` and `FieldKind::LiteralList`
exist as two kinds for one reason: a `Reference` carries an `alias.`
qualifier, a `Literal` carries a `$param`, and neither carries both.
`schema.rs` says so directly. `path_prefix` is a `LiteralList` because
the parser builds a `Reference` through `parse_key`, so nothing can
write a `$param` into one.

**A live gap in ergonomics.** That restriction reaches every reference
list, so **no template can parameterize a network, a middleware, an
entry point, a resolver, or an `env_file` path**. Verified:

```hll
template web(net: String) {
  networks [$net]
}
```

```text
4:13: expected a field name (identifier or string), found `$`
```

Templates are the language's central feature. A value model that blocks
`$param` in five of the fields templates most want to abstract is the
largest single ergonomic cost in this audit.

**Proposal.** One value type across the whole language:

```
Value ::= STRING | NUMBER | IDENT | IDENT "." IDENT | "$" IDENT
```

Whether a given position accepts the *qualified* form becomes a schema
bit the compiler checks after parsing, so `middleware`, `dns`,
`env_file`, and `depends_on` reject it exactly as they do today, rather
than a fork in the parser. The same holds for whether a `$param` is
legal, which is already a context check through
`ParamReferenceOutsideTemplate`, applied inconsistently.

**Deletes.** `FieldKind::LiteralList`, the `Reference`/`Literal` duality
throughout the tree, `reject_qualified`'s bespoke list, and
`parse_reference` plus `parse_bracket_reference_list` as separate
functions from their literal twins.

**Adds.** `$param` works uniformly wherever a value goes.

**Compose output.** Unchanged. The change only widens what parses.

---

### F3—`MergeAcc` carries six bespoke slots because values aren't uniform

**Evidence.** `MergeAcc` has two generic, table-driven paths, `scalars`
with `SCALAR_FIELDS` and `lists` with `LIST_FIELDS`, and then six
hand-written slots beside them:

| Slot | Why it skips the table |
|---|---|
| `healthcheck_test` | `HealthcheckTest` isn't a `Literal` |
| `healthcheck_disable` | bare flag, holds a `Span` rather than a `Literal` |
| `traefik_disabled` | bare flag |
| `command` | `Command` isn't a `Literal` |
| `entrypoint` | `Entrypoint` isn't a `Literal` |
| `privileged` | bare flag |

Each one costs a struct field, a doc paragraph, a `merge_tier` branch,
and a rebuild block in `into_service_fields`. `merge_scalar_like` exists
as a second generic beside `merge_scalar` purely to serve them. The
code's own comment concedes the trajectory—"not worth a second
name-keyed table for four rows"—and the count now stands at six, with
every future `ScalarOrList` or `BoolFlag` field adding another.

`SCALAR_FIELDS`'s doc states the intent that keeps eroding: adding a
future scalar collision point should mean adding one `ScalarField` entry
and touching neither generic function. That's what stopped holding.

**Proposal.** Extend the unified `Value` from F2 with a list arm and a
flag arm. All six slots become ordinary `SCALAR_FIELDS` rows, and
`merge_scalar_like`, the six slots, and the six rebuild blocks collapse
into the existing loop.

**Deletes.** Six `MergeAcc` fields, `merge_scalar_like`,
`FieldKind::ScalarOrList` and `FieldKind::DependsOnList` as merge
strategies, since both remain as *parse* conveniences only, and the
`get_or_insert` span-ordering comments that document a hazard the
table's own ordering creates.

**Result.** Bespoke merge slots drop from 6 to 0, and one `merge_scalar`
serves every scalar-like field.

`FieldKind` itself barely moves, and an earlier draft of this audit
overstated that. `ScalarOrList` and `DependsOnList` look like merge
strategies, but the merge engine never branches on them—they're purely
*parse* descriptors, so they stay. Removing `LiteralList` under F2 takes
the count from 9 to 8, and F3 takes it no further. The win here is the
six slots and the duplicate generic, not the variant count.

**Compose output.** Unchanged.

---

### F4—`volume`, `publish`, and `devices` are three copies of one type

**Evidence.** The service-level `volume` field, `publish`, and `devices`
carry three near-identical pairs of tree types, three `MergeAcc` vectors,
three `merge_map` call sites, and three `TypeSchema` statics. All three
share one shape: `host -> container`, uniqueness on the container side,
and an optional trailing modifier. `DESIGN.md` says so twice, calling
`devices` the closest sibling of `publish`, with the identical separator
and value-side uniqueness for the identical reason.

They differ in two details. The host side of `volume` may name a
declared volume, through `key_may_be_reference`, and the modifier on
`volume` is a `{ read_only }` body while the other two ride a suffix
inside the container string.

**Proposal.** One arrow-map type holding a host value, a container
value, and a modifier slot, with three schema rows pointing at it.
`key_may_be_reference` stays a per-row schema bit.

**Deletes.** Four tree types, two `MergeAcc` vectors, and two schema
statics' worth of duplicated shape.

**Compose output.** Unchanged. This is an internal refactor with no
language-surface change at all, which makes it the lowest-risk item here
and a good first move.

---

### F5—two statement-separator rules, split by type kind

**Evidence.** A struct body separates fields with a **newline only**, and
a comma there is a hard error. A map body—`raw`, `env`, `volume { }`, and
a `with` invocation's argument body—accepts **a comma or a newline**.
Two rules, two error paths, and roughly a page each in `DESIGN.md` and
the book.

```
image "x", restart unless-stopped
```
```text
2:16: expected a newline before the next field, found `,`
```

A user who hits that learns nothing they can generalize. Whether a comma
is legal depends on which of two categories the enclosing type falls
into, and nothing in the syntax marks which.

**Proposal.** One rule: **a comma or a newline separates statements,
everywhere.** The parser already owns the machinery that decides.
`parse_secondary_fields` looks one token past a comma to settle whether
the next key belongs to the current field or to the enclosing body, and
that same decision generalizes.

**A caveat to design around.** `entrypoint` names both a `service` field
and an `expose` sub-field, so `expose 80, entrypoint: web` keeps binding
to `expose`, as it does today. Under a single rule that becomes the
*only* ambiguity left in the grammar rather than one of several, which
argues for the rename in F8.

**Compose output.** Unchanged. The change only widens what parses.

---

### F6—`as` is a general mechanism with exactly one inhabitant

**Evidence.** `TypeSchema::bare_keyword_alias` is generic schema data
with one entry ever: `as` aliasing onto `expose`'s `host`. It buys a
dedicated branch in `parse_struct_primary_shorthand`, a dedicated
exclusion inside `parse_secondary_fields`, a dedicated
`ParseError::AliasSugarCannotContinue`, and a documented dead end, since
`expose 80 as "h", entrypoint: web` is a compile error and the sugar can
never mix with the comma form.

**Proposal.** Keep the *spelling*. It's the shortest way to write a
single-router service, and F1 makes it the canonical one. Drop the
*mechanism*: put the one desugaring directly in F1's lowering and remove
`bare_keyword_alias`, its generic parse branch, its lookahead exclusion,
and `AliasSugarCannotContinue`.

A generalization with one instance is a cost with no return. Should a
second alias ever earn its place, reintroducing the field is a smaller
change than carrying it unused.

**Compose output.** Unchanged.

---

### F7—`raw` discards data silently where every other field errors

**Evidence.** `raw` is the one field the merge concatenates outright,
through `acc.raw.entries.extend`, with no uniqueness check at any tier.
Verified:

```hll
template a { raw { user: "1000" } }
template b { raw { user: "2000" } }
service s {
  with a, b
  image "nginx"
}
```

That emits `user: '2000'`. The identical conflict on `env` raises
`MapKeyCollision` and fails the build.

`raw` is the *escape hatch*, which makes it the field most likely to
appear in a shared template, precisely because it covers what nothing
else does. It's also the one place in the language where composing two
templates loses a value in silence.

**Proposal.** Route `raw` through the same `merge_map` the other map
fields use, keyed on `MapSide::Key`. `TypeSchema::uniqueness` then has no
`None` inhabitants left, so the `Option` goes too.

**Compose output.** Changes only for a program that already loses a value
in silence, which is the point. Everything else stays byte-identical.

---

### F8—`entrypoint` and other names that collide

Three cases, in descending severity:

1. **`entrypoint` names two unrelated things.** Traefik entry points,
   inside `expose` and `router`, and Compose's `ENTRYPOINT` override, on
   `service`. Only position separates them, and `DESIGN.md` spends five
   paragraphs explaining why that's safe. It's safe for the *parser*.
   It isn't safe for a reader. **Rename the Traefik one to
   `entrypoints`**: it's a list, it matches the label Traefik itself
   spells `entrypoints=`, and the collision disappears outright. That
   also clears F5's one caveat.
2. **`healthcheck { disable }` against `traefik { disabled }`.** Two
   spellings of one idea, on adjacent lines of the same service. Pick
   `disable`, matching Compose's own key.
3. **A `volume` declaration against a `volume` mount.** Leave it. These
   are two genuinely distinct Docker concepts that Docker itself spells
   the same way, and the book handles the pair clearly.

F1 makes a fourth case worth revisiting. `traefik { disabled }` takes a
braced struct only because a `FieldKind::BoolFlag` can't serve as a
`primary_field`, and `DESIGN.md` records that the preferred spelling
`traefik disabled` "doesn't fit the schema engine." Letting a bare flag
serve as a primary field is a small, contained generalization that lifts
a restriction the design already regrets.

**Compose output.** Unchanged. These are source-level renames.

---

### F9—what earns a schema row needs an explicit bar

**Evidence.** `dns`, `env_file`, `container_name`, and `driver_opts` are
thin pass-throughs. None validates, interpolates, or reshapes anything,
and none merges in a way a map field wouldn't give for free. Each costs
a schema row, a tree field, a merge-table row, a book section, and
tests. Contrast `expose`, `volume`, `router`, and `depends_on`, each of
which *transforms* its input on the way to Compose.

`DESIGN.md` already carries one promotion test: would this make sense on
a homelab with completely different infrastructure? That test screens for
*genericness*, and all four pass it. It doesn't screen for *value added
over `raw`*, which is the axis the growth actually runs along.

**Proposal.** Add a second bar to the design-principle section of
`DESIGN.md`: **a Compose key earns a row only when the compiler does
something to it**—validates it, interpolates into it, reshapes it, or
merges it non-trivially. Otherwise it belongs in `raw`.

Apply the bar to future additions rather than removing today's fields.
F7 is the prerequisite that makes `raw` a safe destination. The four
fields named here are simply what the bar would have caught.

---

### F10—`template <name> = <statement>` works and no page documents it

**Evidence.** The grammar carries `template_decl ::= "template" IDENT
param_list? ( body | "=" statement )`, the parser implements it, tests
cover it, and it runs:

```hll
template pinned = image "nginx:1.25"
service s { with pinned }
```

The book never mentions it. It's a second declaration form for a case the
braced form covers in the same number of lines, as `template pinned {
image "nginx:1.25" }`.

**Proposal.** Delete it. An undocumented alternate spelling is the worst
of both: surface area the implementation has to keep working and users
can't discover. Documenting it instead is defensible, but that adds a
form to the book for no gain in what the language expresses.

**Compose output.** Unchanged. Nothing in the repo writes the form
outside its own tests.

---

### F11—`Number` and `String` are the whole parameter type system

**Evidence.** `Number` and `String`, checked strictly with no coercion,
plus a third untyped state the compiler checks not at all. `DESIGN.md`
states the ceiling: a parameter typed as a bare `IDENT` or as a list
"isn't expressible yet."

Once F2 lands, `$param` reaches list and reference positions, and a type
system that says only `Number`, `String`, or nothing falls visibly short
of the value model underneath it.

**Proposal.** Settle the direction *with* F2 rather than after. Either
extend the annotations to cover the unified value kinds, or drop
annotations and check at substitution time against the field's own schema
kind, which knows more than the annotation does anyway. The middle
ground is the expensive one: it costs a grammar production, a `ParamType`
enum, `UnknownParamType`, and `ArgumentTypeMismatch` to check two of five
value kinds.

---

## What earns its complexity

Leave these alone. Each does real work.

- **The schema-table-driven parser.** It's why 22 service fields cost one
  parse function and not 22. Every proposal here keeps it, and most push
  more logic into it.
- **Lexically scoped templates and non-transitive imports.** Both are the
  surprise-free semantics, and both are cheap.
- **The three-tier merge, with `defaults` always losing.** Small,
  well-specified, and the reason template composition stays predictable.
- **Hard errors for a router-less `middleware` or `entrypoint`.** These
  catch a real deploy-breaking mistake—a service shipped with its
  forward-auth missing—that a warning wouldn't.
- **The Traefik metacharacter guards, on the key side, and on the value
  side.** Label injection is a genuine hazard, and the two guards use
  correctly *different* character sets.
- **`raw` as the long-tail escape hatch.** The right design. F7 and F9
  make it more load-bearing, not less.
- **One reserved word.** `template` standing alone, with everything else
  contextual, is a real asset, and nothing here disturbs it.

## Suggested sequencing

Each step lands on the one before it, and the risky, user-visible change
sits in the middle rather than first.

| # | Change | Risk | Language change |
|---|---|---|---|
| 1 | **F4**, unify the three arrow-map types | low | none |
| 2 | **F2**, unify `Reference` and `Literal` into one value | medium | widening only |
| 3 | **F3**, collapse the six merge slots onto `SCALAR_FIELDS` | low | none |
| 4 | **F1** with **F6**, split `expose` and `router` by job | high | breaking |
| 5 | **F7** with **F8**, `raw` collision checking and renames | low | breaking |
| 6 | **F5**, one separator rule | medium | widening only |
| 7 | **F9**, **F10**, and **F11**, for policy, and for parameter types | low | mixed |

Steps 1 through 3 are pure internal consolidation and account for most of
the code the audit would delete. Step 4 is the one restructure a user
notices, and step 2 is what makes it worth doing: against a unified value
model, the merged `router` is a smaller and more regular thing than
either half is today.

Expected end state:

- `FieldKind` from 9 to 8, through F2 alone
- Bespoke `MergeAcc` slots from 6 to 0
- Arrow-map tree types from 4 to 2
- Two fewer `ParseError` variants, two fewer `CodegenError` variants
- One separator rule instead of two, one value type instead of two
- `$param` usable in every value position
- One book section deleted and several shortened

Backward compatibility is no constraint before 1.0, but every step except
F1, F7, and F8 is source-compatible anyway, and none of them changes what
a program that compiles today means to Compose.
