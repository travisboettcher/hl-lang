# Templates & Composition

A `template` is a named, reusable partial service — a block of fields
that gets merged onto a real `service` via `with`, rather than a service
in its own right. This is `hll`'s answer to the copy-paste every homelab
accumulates: the shared Traefik network, the forward-auth middleware, the
`PUID`/`PGID` pair every LinuxServer.io image wants, all written once and
pulled in wherever they're needed.

## Declaring a template

A template accepts exactly the same fields as a `service` body (see
[Built-in Fields](./built-in-fields.md)):

```hll
template internal_web(port: Number) {
  networks [traefik-net]
  restart unless-stopped
  expose $port, host: "{{name}}.internal.example.com", entrypoint: "web-secure"
  middleware local-ipwhitelist
}
```

- `(port: Number)` declares the template's parameter list. A parameter
  can optionally be typed `Number` or `String`, checked strictly (a
  `Number` parameter rejects a quoted string, even a numeric-looking
  one). An untyped parameter accepts any literal.
- `$port` inside the body refers to that declared parameter — the `$`
  sigil is reserved for exactly this, and is only legal inside a
  template's own body.
- `{{name}}` interpolates the *calling* service's own name at compile
  time — see [Syntax Basics](./syntax-basics.md#comments-and-interpolation).

A template with no parameters just omits the parameter list:

```hll
template authenticated {
  middleware forwardAuth-authentik
}
```

## Applying a template with `with`

`with` merges one or more templates onto a service:

```hll
service syncthing {
  with internal_web { port: 8384 }, authenticated
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

Each item in a `with` list is a template name, followed by a `{ arg:
value, ... }` argument body if the template takes parameters (a
zero-parameter template like `authenticated` needs no body — bare
`authenticated` is enough). A template must always be fully applied at
each call — there's no partial application or currying.

A template's own body can itself `with` other templates, so templates
compose. A template may also forward its own parameters into the
templates it applies:

```hll
template linuxserver_app(puid: Number, pgid: Number) {
  env PUID = $puid
  env PGID = $pgid
}

template linuxserver_web(puid: Number, pgid: Number, port: Number) {
  with linuxserver_app { puid: $puid, pgid: $pgid }
  expose $port, entrypoint: "web-secure"
}
```

## The implicit `defaults` template

A template named exactly `defaults` is special-cased: if one is declared
in a file, it's applied to every service in that file automatically —
no `with defaults` needed:

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

`defaults` isn't a reserved word — it's an ordinary template name the
compiler recognizes. It also never participates in collision-checking
(below): it always loses silently to anything more specific, which is
exactly what you want from a fallback.

## Merge order and collisions

When a service ends up with fields from more than one source — its own
body, one or more `with`-listed templates, and possibly an implicit
`defaults` — they merge in a fixed priority order, lowest to highest:

1. the implicit `defaults` template, if declared
2. explicit `with`-listed templates, left to right
3. the service's own body — always wins over everything

**A collision between two explicit `with`-listed templates on the same
scalar or map field is a compile error** — if two templates you
explicitly listed both try to set `image`, or both set the same `env`
key, `hllc` won't guess which one you meant. Note that setting the field
in the service's own body does *not* break the tie: the explicit tier
merges to completion before the body is applied, so the collision is
reported first and the body never gets a chance to win. The two real
remedies are to drop one of the templates from the `with` list, or to
refactor the contested field out of one of them. `defaults` is exempt
from this check (it always silently loses), and the service's own body
is exempt too (it always silently wins over whatever survives the
explicit tier).

Different field kinds merge differently:

- **List fields** (`middleware`, `depends_on`, `networks`, `dns`)
  concatenate — no collision is possible, since there's nothing to
  overwrite.
- **Map fields** (`volume`, `env`) merge key-by-key (or value-by-value
  for `volume`, since its uniqueness check is on the container-path
  side) — a genuine collision on the same key is the compile error case
  above.
- **Scalar fields** (`image`, `restart`) error on collision among
  explicit templates only, per the rule above.
- **`expose`** is the one built-in struct field with more than one
  sub-field, and merges per sub-field (`port`/`host`/`entrypoint`
  independently) rather than as one indivisible unit — the same
  key-by-key reasoning as a map field, applied to a struct's named
  fields instead of a map's keys.

That last point means a service's own body can override just
`expose.host` while still inheriting `port`/`entrypoint` from a
`with`-listed template, without repeating them:

```hll
service it-tools {
  with internal_web { port: 8080 }
  image "corentinth/it-tools:latest"
  # overrides just expose.host — port and entrypoint still come from
  # internal_web
  expose { host: "tools.internal.example.com" }
}
```

## A complete example

Putting it together — a network, three templates, and a service that
composes all three:

```hll,build
network traefik-net {
  external
  name: "docker_default"
}

template internal_web(port: Number) {
  networks [traefik-net]
  restart unless-stopped
  expose $port, host: "{{name}}.internal.example.com", entrypoint: "web-secure"
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
  volume "syncthing-config" -> "/config"
}
```

`syncthing` ends up with: a network reference and `restart` from
`internal_web`; an `expose` block built from `internal_web`'s `port`
parameter plus its own `{{name}}`-interpolated host; a middleware entry
each from `internal_web` and `authenticated`; two `env` entries from
`linuxserver_app`; and its own `image`/`volume`, which no template set.

Once these templates start getting reused across more than one `.hll`
file, the next step is pulling them into a shared file and `use`-ing them
— see [Imports](./imports.md).
