# Built-in Fields

This is a reference for every field `hll` understands: what it accepts,
its default, and what it produces in the generated Compose YAML. See
[Syntax Basics](./syntax-basics.md) for the shorthand forms referenced
below (primary-value, secondary-field, map-style).

## `service` and `network`

`service` and `network` are the two top-level declaration types — both
require a name (`service jellyfin { ... }`, `network traefik-net { ...
}`) and a body. A `template` body accepts exactly the same set of fields
as `service` — see [Templates & Composition](./templates-and-composition.md).

### `network` fields

| Field | Accepts | Default |
|---|---|---|
| `external` | bare flag (no value) | unset (`false`) |
| `name` | string | the network's own `hll` identifier |

`external` marks a network as one Docker already manages (e.g. one
`docker compose` created for another stack) rather than one this file's
own Compose output should create. `name` is the real underlying Docker
network name, when it differs from the identifier you declared the
network under — needed because Compose's own auto-derived network names
depend on the directory a `docker compose` was run from, which the
compiler can't know:

```hll
network traefik-net {
  external
  name: "docker_default"
}
```

## `image`

Primary field: `ref`.

| Field | Accepts | Default |
|---|---|---|
| `ref` | string | *(required — no default)* |

```hll,fragment
image "jellyfin/jellyfin:latest"
```

Every service needs an `image` — either directly or inherited from a
template — `hllc --build` fails if one is missing.

## `expose`

Primary field: `port`. Secondary-field shorthand: `as` aliases to `host`.

| Field | Accepts | Default |
|---|---|---|
| `port` | number | *(no default — omitting `expose` entirely just means no Traefik routing)* |
| `host` | string | unset (no router rule generated) |
| `entrypoint` | string | unset (label omitted; Traefik attaches the router to every entry point) |

```hll,fragment
expose 8096 as "media.example.com"
# same as:
# expose {
#   port: 8096
#   host: "media.example.com"
# }
```

`as` is a one-shot fusion, not a list — it can't be followed by more
fields. To also set `entrypoint`, name `host` explicitly instead:

```hll,fragment
expose 8096, host: "media.example.com", entrypoint: "web-secure"
```

`expose.port` becomes Compose's `expose:` entry (the port is reachable
from other containers on the same network — it isn't published to the
host). `expose.host`, if set, generates a Traefik router-rule label
(`Host(...)`,) routing that hostname to this service; `expose.entrypoint`,
if set, restricts that router to one named Traefik entry point instead of
all of them.

## `volume`

Map-kind. Bare-entry separator: `->` (host path/volume name → container
path). Uniqueness is checked on the **container path** (the value side)
— Docker itself refuses two mounts at the same container path, but
allows the same host path mounted more than once.

```hll,fragment
volume "/mnt/media" -> "/data"        # bind mount
volume "syncthing-config" -> "/config" # named volume
```

Repeating `volume` accumulates entries rather than overwriting. A host
side that isn't an absolute path is treated as a named Docker volume and
added to the Compose document's top-level `volumes:` section
automatically.

## `env`

Map-kind. Bare-entry separator: `=` (key = value). Uniqueness is checked
on the **key** — two `env` entries can't set the same variable.

```hll,fragment
env PUID = "1000"
env PGID = "100"
```

Repeating `env` accumulates entries.

## `restart`

Primary field: `policy`.

| Field | Accepts | Default |
|---|---|---|
| `policy` | bare word or string | unset (Compose's own default — no automatic restart) |

```hll,fragment
restart unless-stopped
```

Writing `image` or `restart` more than once in the same body is a
compile error (both are scalar fields, not repeatable) — unlike
`volume`/`env`/`middleware`/`depends_on`.

## `middleware`, `depends_on`, `networks`, `dns`

All four are plain reference-list fields directly on `service`/`template`
— not nested struct types, so there's no primary-field shorthand to
learn for them; write a bare identifier, a bracketed list, or repeat the
field:

```hll,fragment
middleware local-ipwhitelist
middleware forwardAuth-authentik   # repeating accumulates

depends_on database

networks [traefik-net]

dns ["192.168.50.182"]
```

- `middleware` names a Traefik middleware to attach to this service's
  router (generates a `traefik.http.routers.<name>.middlewares=` label
  entry per item).
- `depends_on` names a same-file sibling `service` this one depends on —
  it's not cross-file, and doesn't accept a qualified `alias.name`.
- `networks` references a top-level `network` declared in the same
  program (see above). If exactly one referenced network is `external`,
  its real name also drives the `traefik.docker.network=` label; more
  than one `external` network on the same service is a compile error
  (ambiguous which one Traefik should target).
- `dns` sets Compose's own per-service `dns:` key — a resolver override,
  e.g. for a network with a local DNS server.

All four accumulate across repeated writes and across template
composition (see [Templates & Composition](./templates-and-composition.md)) —
there's no collision to check since list fields can only ever grow.

## `container_name`

A plain scalar field directly on `service`/`template` (not a nested
struct type):

```hll,fragment
container_name "uptime-kuma"
```

| Accepts | Default |
|---|---|
| string | the service's own name |

## `raw`

Map-kind, schema-free: unknown keys are accepted as-is rather than
checked against a fixed field list, and values pass straight through to
the generated YAML. This is the escape hatch for any Compose key `hll`
doesn't have a dedicated field for yet:

```hll,fragment
raw {
  privileged: true,
  cap_add: ["NET_ADMIN"]
}
```

Each `raw` entry becomes a sibling top-level key on the generated
Compose service block (`privileged: true`, `cap_add: [...]`), exactly as
written — there's no validation, so a typo'd key or a value Compose
doesn't understand won't be caught until `docker compose` itself rejects
it.

## `with`

Not really a "field" you set directly so much as the mechanism for
pulling a `template`'s fields onto a `service` — see [Templates &
Composition](./templates-and-composition.md) for `with` in full.
