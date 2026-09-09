# Templates & composition

A `template` is a named, reusable partial service—a block of fields
that gets merged onto a real `service` via `with`, rather than a service
in its own right. This is `hll`'s answer to the copy-paste every homelab
accumulates: the shared Traefik network, the forward-auth middleware, the
`PUID`/`PGID` pair every LinuxServer.io image wants, all written once and
pulled in wherever they're needed.

## Declaring a template

A template accepts exactly the same fields as a `service` body (see
[Built-in Fields](./built-in-fields.md)):

```hll
template internal_web(port) {
  networks [traefik-net]
  restart unless-stopped
  expose $port
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{name}}.internal.example.com`)"
    "traefik.http.routers.{{name}}.entrypoints": "web-secure"
    "traefik.http.routers.{{name}}.middlewares": ["local-ipwhitelist@file"]
  }
}
```

- `(port)` declares the template's parameter list—just bare names, with
  no type annotation to write. Composition checks a substituted argument
  against the field it lands in instead: `$port` in the preceding
  example reaches `expose.port`, one of the fields
  [Built-in Fields](./built-in-fields.md) documents as taking a
  `number`, so `with internal_web { port: "8384" }` is a compile error
  even though nothing here declared `port: Number`. A reference-shaped
  field like `networks` rejects a bare number the same way—see
  [Parameterizing references](#parameterizing-references) below—while
  every other field takes whatever literal kind its argument happens to
  be.
- `$port` inside the body refers to that declared parameter—the `$`
  sigil serves exactly this purpose, and works only inside a
  template's own body. It fills a whole value. To put a parameter
  *inside* a string, see
  [Interpolating a parameter](#interpolating-a-parameter-into-a-string)
  further down this page.
- `{{name}}` interpolates the *calling* service's own name at compile
  time—see [Syntax Basics](./syntax-basics.md#comments-and-interpolation).

A template with no parameters just omits the parameter list:

```hll
template authenticated {
  labels {
    "traefik.http.routers.{{name}}.middlewares": ["forwardAuth-authentik@file"]
  }
}
```

Both templates write the same `labels` key, and because each writes a
*list* the two concatenate in tier order rather than colliding—see
[Merge order and collisions](#merge-order-and-collisions) below.

## Applying a template with `with`

`with` merges one or more templates onto a service:

```hll
volume syncthing-config {}

service syncthing {
  with internal_web { port: 8384 }, authenticated
  image "lscr.io/linuxserver/syncthing:latest"
  volume syncthing-config -> "/config"
}
```

Each item in a `with` list is a template name, followed by a `{ arg:
value, ... }` argument body if the template takes parameters (a
zero-parameter template like `authenticated` needs no body—bare
`authenticated` is enough). A template must always be fully applied at
each call—you can't partially apply it or curry it.

A template's own body can itself `with` other templates, so templates
can layer on each other—up to 64 levels of nesting, past which `hllc`
reports an error instead of following the chain further. That's a bound
on `with` *depth*, not on how many templates a single `with` list may
name.

A template may also forward its own parameters into the templates it
applies:

```hll
template linuxserver_app(puid, pgid) {
  env PUID = $puid
  env PGID = $pgid
}

template linuxserver_web(puid, pgid, port) {
  with linuxserver_app { puid: $puid, pgid: $pgid }
  expose $port
  labels {
    "traefik.http.routers.{{name}}.entrypoints": "web-secure"
  }
}
```

## Parameterizing references

`$param` isn't limited to plain values like the preceding `$port`—you
can write it anywhere you write a reference too, so a template can
parameterize which network it attaches to, which middleware it names, or
which entry point it routes through, not just the values on its other
fields:

```hll,build
network proxy {
  name: "real-proxy"
}

template attached_to(net) {
  networks [$net]
}

service app {
  image "nginx:alpine"
  with attached_to { net: "proxy" }
}
```

Composition checks the substituted argument against `networks`' own
grammar before it goes anywhere near name resolution: `with attached_to
{ net: 1000 }` is a compile error, since a bare number can never appear
in a reference-shaped position even written directly. Past that, the
argument still has to name something real: `with attached_to { net:
"ghost" }` fails with the same `UnknownNetwork` error `networks [ghost]`
written directly would, since resolving a network by name happens after
composition binds the parameter, not before.

## Interpolating a parameter into a string

`$param` fills a whole value. When what you need is a parameter in the
*middle* of one—a hostname inside a routing rule, a name inside a
dotted label key—write `{{param}}` instead, the same interpolation form
`{{name}}` uses:

```hll,build
template traefik_http(host, port) {
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{host}}`)"
    "traefik.http.services.{{name}}.loadbalancer.server.port": $port
  }
}

service jellyfin {
  image "jellyfin/jellyfin"
  with traefik_http { host: "media.example.com", port: 8096 }
}
```

```yaml
labels:
- traefik.http.routers.jellyfin.rule=Host(`media.example.com`)
- traefik.http.services.jellyfin.loadbalancer.server.port=8096
```

Both bindings appear in that template and they resolve at different
times, which is worth knowing when one of them goes wrong:

- `{{param}}` resolves as the template merges onto a service, against
  that invocation's arguments.
- `{{name}}` resolves later, against the service the result lands on. A
  template *may* declare a parameter named `name`, but `{{name}}` goes
  on meaning the service and `hllc` says so. Reach that parameter as
  `$name` instead.

A binding naming neither raises `unknown interpolation {{hsot}}`, so a
typo stops the build rather than reaching the output.

An argument can be anything with a text form—a string, a number, a bare
identifier, or another parameter forwarded from the enclosing template.
A list or a nested map has no text to splice into a string, so passing
one to a `{{param}}` fails to compile, even though the same argument
still fills a whole slot that accepts a list.

### `$param` inside a string does nothing

The `$` sigil never reaches inside string content:

```hll
template traefik_http(host) {
  labels {
    # Wrong: emits the five characters `$host`, not the argument.
    "traefik.http.routers.{{name}}.rule": "Host(`$host`)"
  }
}
```

That compiles, and writes ``rule=Host(`$host`)`` into the generated
file—a router matching a host literally named `$host`, which nothing
ever requests. `hllc` warns when a string inside a template holds a `$`
naming one of that template's own parameters. The fix is the preceding
`{{host}}` spelling.

The warning stays deliberately narrow. A `$` in any other string is
ordinary content: `command` and `env` values carry `$HOME` through to a
shell, and Compose reads its own `${VAR}` interpolation out of the
generated file once `hllc` has written it. The warning skips both.

### Passing a list

A list argument does one of two things, depending on where the parameter
sits.

**Interpolated into a string, it joins with commas.** That's the shape
of a Docker label holding several entries:

```hll,build
template middlewares(router, chain) {
  labels {
    "traefik.http.routers.{{router}}.middlewares": "{{chain}}"
  }
}

service web {
  image "nginx"
  with middlewares {
    router: "{{name}}",
    chain: ["auth@file", "compress@file"]
  }
}
```

```yaml
labels:
- traefik.http.routers.web.middlewares=auth@file,compress@file
```

Any item with a text form counts—a quoted string, a number, a bare
identifier, a forwarded parameter, a `decl.name` field access—so
`[1, "two", three]` renders `1,two,three`. The comma is the only
separator there is. A template that needs a different one takes the
joined string as an ordinary parameter instead.

**In a list-shaped field, it splices.** The items land where the
parameter stood:

```hll,build
network a { }
network b { }
network c { }

template attach(nets) {
  networks [a, $nets, c]
}

service web {
  image "nginx"
  with attach { nets: [b] }
}
```

```yaml
networks:
- a
- b
- c
```

`networks $nets` and `networks [$nets]` mean the same thing as each
other, since a bare list field and a one-element bracket list parse
alike. The same goes for the other list fields alongside it—`dns`,
`env_file` and `depends_on`. A `depends_on` entry carries a condition as
well as a name, and every item spliced through that entry takes it:

```hll,build
service db { image "postgres" }
service cache { image "redis" }

template waits_for(deps) {
  depends_on [$deps { condition: service_healthy }]
}

service web {
  image "nginx"
  with waits_for { deps: [db, cache] }
}
```

```yaml
depends_on:
  db:
    condition: service_healthy
  cache:
    condition: service_healthy
```

An empty list means what it says in both places: no characters when
joined, no elements when spliced.

Two things a list can't do. It can't nest—`[a, [b]]` is an error rather
than a flattening, because `[a, b]` already spells the flat list and one
source shouldn't have two spellings. And it can't fill a slot that holds
a single value, so `container_name $xs` is an error however many items
`xs` holds:

```hll,ignore
container_name $xs
```

```text
2:18: argument `xs` for template `t` must be a scalar value (a list/map can't fill a single-value field)
```

### An interpolated value lands as written

`{{host}}` puts the argument into the string as it stands. Nothing
inspects it on the way, so a template that renders a Traefik rule from a
parameter trusts whoever calls it:

```hll
template traefik_http(host) {
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`{{host}}`)"
  }
}
```

Pass ``ok.example.com`) || HostRegexp(`{any:.+}`` as `host` and the rule
matches every host instead of one. `hllc` can't catch that, because it
no longer knows the label holds a rule—see [a value goes through as
written](./built-in-fields.md#a-value-goes-through-as-written). A
template that splices a parameter into a value with a syntax of its own
owns that syntax.

## Reading a declaration's real name

A `network` or `volume` answers to two names: the identifier your `.hll`
files refer to it by, and the real Docker name—the `name:` override when
the declaration sets one, the identifier otherwise. A label that has to
name a real Docker network needs the second. Write `.name` after the
declaration to read it:

```hll,build
network proxy {
  external
  name: "docker_default"
}

template caddy(net, port) {
  networks [$net]
  expose $port
  labels {
    "caddy.network": $net.name
    "caddy.upstream": "{{name}}:{{port}}"
  }
}

service jellyfin {
  image "jellyfin/jellyfin"
  with caddy { net: proxy, port: 8096 }
}
```

```yaml
networks:
- proxy
expose:
- 8096
labels:
- caddy.network=docker_default
- caddy.upstream=jellyfin:8096
```

One parameter serves both positions, and each takes what it needs:
`networks [$net]` attaches the network, so it wants the identifier,
while the label hands the network's name to the proxy that reads it, so
it wants `docker_default`. Passing the identifier to both is the trap
this replaces—it compiles, and the label reads `caddy.network=proxy`,
which matches nothing. Drop the `name:` override and the two spellings
agree, so the mistake survives a small test and breaks on exactly the
files that need it: an external network another Compose project created
almost always carries a `name:`.

Three spellings read the same field:

| written in a value | reads |
| --- | --- |
| `proxy.name` | a `network`/`volume` this program declares |
| `net.proxy.name` | one an [imported file](./imports.md) declares |
| `$net.name` | whichever declaration the invocation binds `net` to |

The last row is what makes the parameter worth having, and it takes an
imported declaration as readily as a same-file one: `with caddy { net:
shared.proxy }` binds the declaration `shared.hll` declares, and
`$net.name` reads `docker_default` off it just the same. See [Passing an
imported declaration to a
template](./imports.md#passing-an-imported-declaration-to-a-template).

A field is readable when it holds a value. `name` holds one on both
kinds, and a `volume`'s `driver` holds one too:

| written in a value | reads |
| --- | --- |
| `media.name` | the volume's real Docker name |
| `media.driver` | the driver it names, when it sets one |

The fields that hold nothing say so rather than pretending not to
exist. `external` is a bare-presence flag and `driver_opts` is a map, so
neither fills a value, and `hllc` names the field and why. Ask for a
field the kind hasn't got at all and it lists the ones it has. Ask for
one the declaration leaves unset—a `volume` with no `driver`—and it
refuses rather than handing back an empty string, since Docker picks the
default and no text spells the default it picks.

The access also goes inside string content, as a dotted binding
alongside the `{{param}}` form:

```hll,fragment
labels { "caddy.upstream": "http://{{net.name}}:8096" }
```

Two rules keep this unambiguous, and both are worth knowing before you
hit them:

- **Value positions only.** `networks [...]`, `dns`, `env_file`,
  `depends_on`, and a named-volume mount's host side all name a
  *declaration*, and a `.`
  there already qualifies that name by an import alias. Writing
  `networks [$net.name]` is an error that says so.
- **At most three parts.** Two name a declaration and a field, three
  name an alias, a declaration and a field, and `$param` takes exactly
  one field. A fourth part has no reading left.

A `with`-invocation's argument is the one value position where two parts
may instead name an imported *declaration*, since a parameter is the one
value that can go on to be a reference. A base naming one of this
program's own declarations still reads as a field access there, so
nothing you already write changes meaning. Only a base naming nothing
local takes the import-alias reading.

## A worked set of templates: routing

Routing is the biggest thing templates do in a real homelab, and it's
entirely templates—`hllc` bundles a module of them, `std:traefik`, and
has no routing built in at all. It's a good read once this page
makes sense, because it exercises every feature here at once:
parameters, interpolation into a string, list arguments, reading a
declaration's name, and list-valued labels composing across tiers.

[Routing](./routing.md) covers it.


## Every template needs a `with`

A template reaches a service only through that service's own `with`.
No template name is special to the compiler, `defaults` included:

```hll,build
template defaults {
  restart unless-stopped
}

service jellyfin {
  with defaults
  image "jellyfin/jellyfin:latest"
  expose 8096
}
```

`hllc` used to apply a template named exactly `defaults` to every
service in its file, with no `with` needed. That's gone. It only ever
worked within one file—having no invocation left no alias for a
cross-file lookup to go through—so the case it looked like it saved you
from, sharing one baseline across your whole homelab, was the one case
it couldn't serve. Writing `with defaults` costs a line per service and
works everywhere, [imported files](./imports.md) included.

A `template defaults` that no service applies is a warning rather than a
silent no-op, since a file written against the old behavior would
otherwise stop picking those fields up without saying so.

## Merge order and collisions

When a service ends up with fields from more than one source—its own
body and one or more `with`-listed templates—they merge in a fixed
priority order, lowest to highest:

1. `with`-listed templates, left to right
2. the service's own body—always wins over everything

**A collision between two `with`-listed templates on the same
scalar or map field is a compile error**—if two templates you
listed both try to set `image`, or both set the same `env`
key, `hllc` won't guess which one you meant. Note that setting the field
in the service's own body does *not* break the tie: the template tier
merges to completion before `hllc` applies the body, so it reports the
collision first, and the body never gets a chance to win. The two real
remedies are to drop one of the templates from the `with` list, or to
refactor the contested field out of one of them. The service's own
body is exempt from this check, because it always silently wins over
whatever survives the template tier.

Different field kinds merge differently:

- **List fields** (`middleware`, `networks`, `dns`, `env_file`) concatenate—no
  collision is possible, since there's nothing to overwrite. All but `dns`
  and `env_file` concatenate
  *by distinct name*: naming the same network in a template and again in
  the service's own body means what naming it once means, so `hllc`
  drops the repeat rather than emitting it twice. The first occurrence
  is the one kept, so the surviving order is still each `with` target
  left to right, then the body's own entries. `dns`
  and `env_file` are the exception and keep every entry, duplicates
  included, because their order is observable—resolver priority for
  `dns`, Compose's own last-file-wins precedence for `env_file`.
- **`depends_on`** looks like a list field—`depends_on [db]`—but merges
  like the map fields just below it, keyed on the referenced service's
  own name, so the service's own body always wins over a template's
  entry for the same dependency. Unlike the true map fields, though,
  naming the same service twice isn't automatically a collision: two
  entries agree when their conditions match—including a bare entry and
  an explicit `condition: service_started`, which mean the same thing to
  Compose—and two templates that agree are giving the same answer twice,
  not two different ones, so they still collapse to a single entry
  exactly as a plain `depends_on [db]` always has. Only when two
  explicit templates' conditions genuinely *differ* is it the same
  `MapKeyCollision` compile error two templates setting the same `env`
  key to two different values would raise.
- **Map fields** (`volume`, `env`, `labels`, `raw`) merge key-by-key (or
  value-by-value for `volume`, since its uniqueness check is on the
  container-path side)—a genuine collision on the same key, regardless
  of whether the two values happen to agree, is the preceding compile
  error case. The preceding entry's `depends_on` keys like a map field
  too, but its collision check also looks at the *value*: two entries that
  agree aren't a real collision the way two `env` entries sharing a key
  always are, whatever those two entries' values happen to be.
- **Scalar fields** (`image`, `restart`, `expose`'s `port`) error on
  collision among explicit templates only, per the preceding rule.
- **`healthcheck`** is the built-in struct field with more than one
  sub-field, and it merges per sub-field independently rather than as
  one indivisible unit—the same key-by-key reasoning as a map field,
  applied to a struct's named fields instead of a map's keys. Each
  sub-field then follows its own kind's rule: every sub-field but `test`
  is a scalar and collides like `expose.port` does, and `test` collides
  the same way even though its value isn't a plain string or
  number—see below.

  `healthcheck.test` and `healthcheck.disable` collide the same way a
  scalar sub-field does, even though neither is a plain `Literal`:
  `test` carries Compose's own shell-string-or-exec-list shape, and
  `disable` is a bare-presence flag whose only "value" is that it's
  present at all. Two explicit templates each setting `test` (or each
  setting `disable`) still collide, exactly as two explicit templates
  each setting `expose.port` do.
- **`command`** merges the same way `healthcheck.test` does, not the way
  `container_name` does: its shell-string-or-exec-list shape isn't a
  plain `Literal` either, so it collides between two explicit templates
  by the same rule rather than riding the plain-scalar
  machinery `image`/`restart`/`container_name` use. Unlike
  `healthcheck.test`, `command` sits directly on the service body rather
  than inside a struct field of its own, so there's no sub-field
  independence to it—setting `command` at all is the whole collision,
  the same as setting `container_name` is.
- **`entrypoint`** merges exactly the way `command` does, and for the
  same reasons—a service's own value replaces an inherited one, and two
  explicit templates that each set it collide.
  The two are separate Compose keys, though, so they don't collide with
  *each other*: a template that sets `entrypoint` and a template that
  sets `command` merge cleanly, and the service gets both.

- **`labels`** merges key by key like any map field, but the *shape* of
  a value decides what a second contributor means. A single value says
  the key holds one thing, so two explicit templates setting it collide.
  A list says the key holds several, so they concatenate in tier order,
  dropping a repeat—which is what lets a template add one middleware to
  whatever it's mixed into. One key written as a list in one place and a
  single value in another is its own error, since the two disagree about
  what the key holds.

  A shared middleware is exactly what a list-valued `labels` entry is
  for: name it once in a template and every service composing that
  template gets it, with each service free to add its own on top—see
  [A list value composes instead of
  colliding](./built-in-fields.md#a-list-value-composes-instead-of-colliding).

A service's own body still wins over a template for a single-valued
entry, so it can override one routing label while inheriting the rest:

```hll
service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just the rule—the entrypoints and middlewares entries
  # still come from internal_web
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`tools.internal.example.com`)"
  }
}
```

## A complete example

Putting it together—a network, a named volume, three templates, and a
service that composes all three:

```hll,build
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
    "traefik.http.routers.{{name}}.rule": "Host(`{{name}}.internal.example.com`)"
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

`syncthing` ends up with:

- a network reference and `restart` from `internal_web`
- an `expose` block built from `internal_web`'s `port` parameter, and
  its routing labels with their `{{name}}`-interpolated host
- a middlewares entry contributed by both `internal_web` and
  `authenticated`, joined into one label because each wrote a list
- two `env` entries from `linuxserver_app`
- its own `image` and `volume`, which no template sets

Once these templates start getting reused across more than one `.hll`
file, the next step is pulling them into a shared file and `use`-ing
them—see [Imports](./imports.md).
