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
  router {
    host: "{{name}}.internal.example.com"
    entrypoints: web-secure
    middleware: local-ipwhitelist
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
  template's own body.
- `{{name}}` interpolates the *calling* service's own name at compile
  time—see [Syntax Basics](./syntax-basics.md#comments-and-interpolation).

A template with no parameters just omits the parameter list:

```hll
template authenticated {
  router {
    middleware: forwardAuth-authentik
  }
}
```

Both templates name the *unnamed* router, so composing them merges the
two blocks under that one key and their `middleware` lists concatenate
in tier order—see [`router`](#the-merge-rules-field-by-field) below.

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
  router { entrypoints: web-secure }
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

## The implicit `defaults` template

A template named exactly `defaults` is special-cased: if a file declares
one, `hllc` applies it to every service in that file automatically—no
`with defaults` needed:

```hll,build
template defaults {
  restart unless-stopped
}

service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.example.com"
  # restart unless-stopped comes from `defaults` for free
}
```

`defaults` isn't a reserved word—it's an ordinary template name the
compiler recognizes. It also never participates in the collision-checking
described in the next section: it always loses silently to anything more
specific, which is exactly what you want from a fallback.

## Merge order and collisions

When a service ends up with fields from more than one source—its own
body, one or more `with`-listed templates, and possibly an implicit
`defaults`—they merge in a fixed priority order, lowest to highest:

1. the implicit `defaults` template, if declared
2. explicit `with`-listed templates, left to right
3. the service's own body—always wins over everything

**A collision between two explicit `with`-listed templates on the same
scalar or map field is a compile error**—if two templates you
explicitly listed both try to set `image`, or both set the same `env`
key, `hllc` won't guess which one you meant. Note that setting the field
in the service's own body does *not* break the tie: the explicit tier
merges to completion before `hllc` applies the body, so it reports the
collision first, and the body never gets a chance to win. The two real
remedies are to drop one of the templates from the `with` list, or to
refactor the contested field out of one of them. `defaults` is exempt
from this check because it always silently loses, and the service's own
body is exempt too because it always silently wins over whatever
survives the explicit tier.

Different field kinds merge differently:

- **List fields** (`middleware`, `networks`, `dns`, `env_file`) concatenate—no
  collision is possible, since there's nothing to overwrite. All but `dns`
  and `env_file` concatenate
  *by distinct name*: naming the same network in a template and again in
  the service's own body means what naming it once means, so `hllc`
  drops the repeat rather than emitting it twice. The first occurrence
  is the one kept, so the surviving order is still `defaults`, then
  each `with` target left to right, then the body's own entries. `dns`
  and `env_file` are the exception and keep every entry, duplicates
  included, because their order is observable—resolver priority for
  `dns`, Compose's own last-file-wins precedence for `env_file`—see
  #154.
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
  error case. `raw` followed this rule only starting in #193—before
  that it was the one field the merge concatenated outright at every
  tier, so two explicit templates setting the same `raw` key compiled
  with the second one's value silently winning instead of raising a
  collision—see [`raw`'s own section](./built-in-fields.md#raw) for that
  history. The preceding entry's `depends_on` keys like a map field too,
  but its collision check also looks at the *value*: two entries that
  agree aren't a real collision the way two `env` entries sharing a key
  always are, whatever those two entries' values happen to be.
- **Scalar fields** (`image`, `restart`, `expose`'s `port`) error on
  collision among explicit templates only, per the preceding rule.
- **`healthcheck` and `router`** are the built-in struct fields with
  more than one sub-field, and both merge per sub-field independently
  rather than as one indivisible unit—the same key-by-key reasoning as
  a map field, applied to a struct's named fields instead of a map's
  keys. Each sub-field then follows its own kind's rule. Every
  `healthcheck` sub-field but `test` is a scalar and collides like
  `expose.port` does, and `test` collides the same way even though its
  value isn't a plain string or number—see below. A `router`'s `host`
  is a scalar and collides, while its `entrypoints` is a list and
  concatenates, so two explicit templates each naming one entry point
  produce a router attached to both—and two naming the *same* entry
  point produce a router attached to it once, per the preceding
  distinct-name rule.

  `healthcheck.test` and `healthcheck.disable` collide the same way a
  scalar sub-field does, even though neither is a plain `Literal`:
  `test` carries Compose's own shell-string-or-exec-list shape, and
  `disable` is a bare-presence flag whose only "value" is that it's
  present at all. Two explicit templates each setting `test` (or each
  setting `disable`) still collide, exactly as two explicit templates
  each setting `expose.port` do.
- **`command`**, added in #156, merges the same way `healthcheck.test` does, not
  the way `container_name` does: its shell-string-or-exec-list shape
  isn't a plain `Literal` either, so it collides between two explicit
  templates by the same rule rather than riding the plain-scalar
  machinery `image`/`restart`/`container_name` use. Unlike
  `healthcheck.test`, `command` sits directly on the service body rather
  than inside a struct field of its own, so there's no sub-field
  independence to it—setting `command` at all is the whole collision,
  the same as setting `container_name` is.
- **`entrypoint`**, added in #183, merges exactly the way `command`
  does, and for the same reasons—a service's own value replaces an
  inherited one, and two explicit templates that each set it collide.
  The two are separate Compose keys, though, so they don't collide with
  *each other*: a template that sets `entrypoint` and a template that
  sets `command` merge cleanly, and the service gets both.

- **`router`** merges keyed by router name, and then per sub-field
  within each name—both halves of the two preceding rules, one nested
  inside the other. Keyed, so a template's `router api` and a service's `router
  web` give the service two routers rather than one. Per sub-field, so a
  service body writing `router api { host: "..." }` over a template's
  `router api { entrypoints: web-secure, path_prefix: [...] }` keeps the
  entry point and the prefixes it didn't mention. Within one name,
  `host` is a scalar and collides between two explicit templates,
  `entrypoints` and the router's own `middleware` concatenate by distinct
  name like the service-level `middleware`, and `path_prefix`
  concatenates keeping duplicates like `dns`, since the prefixes are
  alternatives whose order is observable in the emitted rule. A
  collision names the router as well as the field, since a message about
  `router.host` alone doesn't say which router.

  A shared middleware is exactly what a template is for: name it once
  in a template's `router` block and every service that composes that
  template gets it, with each service free to add its own on top—see
  [`middleware`](./built-in-fields.md#middleware).

That last point about per-sub-field merging means a service's own body
can override just a router's `host` while still inheriting its
`entrypoint` from a `with`-listed template, without repeating it:

```hll
service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just the unnamed router's host—its entry points still come
  # from internal_web
  router { host: "tools.internal.example.com" }
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
  router {
    host: "{{name}}.internal.example.com"
    entrypoints: web-secure
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

`syncthing` ends up with:

- a network reference and `restart` from `internal_web`
- an `expose` block built from `internal_web`'s `port` parameter, and an
  unnamed `router` block with its `{{name}}`-interpolated host
- a middleware entry each from `internal_web` and `authenticated`, both
  landing on that same unnamed router
- two `env` entries from `linuxserver_app`
- its own `image` and `volume`, which no template sets

Once these templates start getting reused across more than one `.hll`
file, the next step is pulling them into a shared file and `use`-ing
them—see [Imports](./imports.md).
