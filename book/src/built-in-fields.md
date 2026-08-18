# Built-in Fields

This is a reference for every field `hll` understands: what it accepts,
its default, and what it produces in the generated Compose YAML. See
[Syntax Basics](./syntax-basics.md) for the shorthand forms referenced
below (primary-value, secondary-field, map-style).

## `service`, `network` and `volume`

`service`, `network` and `volume` are the three top-level declaration
types — each requires a name (`service jellyfin { ... }`, `network
traefik-net { ... }`, `volume syncthing-config { ... }`) and a body. A
`template` body accepts exactly the same set of fields as `service` —
see [Templates & Composition](./templates-and-composition.md).

Note that `volume` names two different things depending on where it's
written: at the top level it *declares* a named Docker volume (this
section), while inside a `service`/`template` body it *mounts* one — the
map-kind [`volume` field](#volume) further down.

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

### `volume` declaration fields

| Field | Accepts | Default |
|---|---|---|
| `external` | bare flag (no value) | unset (`false`) |
| `name` | string | the volume's own `hll` identifier |
| `driver` | string | unset (Compose's own default, `local`) |
| `driver_opts` | map body (`key: value`) | empty |

`external` and `name` mean exactly what they mean on a `network`: the
first marks a volume Docker already manages rather than one this file's
Compose output should create, the second is the real underlying Docker
volume name when it differs from the identifier you declared it under.
`driver`/`driver_opts` are passed straight through to Compose:

```hll
volume syncthing-config {}

volume media {
  external
  name: "media_store"
}

volume backups {
  driver "local"
  driver_opts {
    type: "nfs"
    o: "addr=192.168.50.10,rw"
    device: ":/exports/backups"
  }
}
```

Every named volume a service mounts must have one of these — see the
[`volume` field](#volume) below for what counts as a named volume and
why the declaration is required.

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
| `entrypoint` | reference list | empty (label omitted; Traefik attaches the router to every entry point) |

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
expose 8096, host: "media.example.com", entrypoint: web-secure
```

`entrypoint` is a **reference list**, spelled exactly like `middleware`
below — a bare name, several comma-separated names, or a bracketed list:

```hll,fragment
expose {
  port: 8096
  host: "media.example.com"
  entrypoint: web, web-secure
}
```

However many entry points you name, they produce **one** label:
`traefik.http.routers.<service>.entrypoints=` with the names
comma-joined (`entrypoints=web,web-secure`) — `hllc` writes the commas,
you write the names. Leave `entrypoint` off entirely and no
`entrypoints=` label is generated at all, which is Traefik's own way of
saying "attach this router to every entry point".

One caveat if you write a bare list in the `expose 8096, ...` shorthand
form: the list ends at the next `field:`, so
`expose 8096, entrypoint: web, host: "media.example.com"` sets one entry
point and a host, not two entry points. Put `entrypoint` last, use
brackets (`entrypoint: [web, web-secure]`), or use the `expose { ... }`
body if you want a bare list in the middle.

`expose.port` becomes Compose's `expose:` entry (the port is reachable
from other containers on the same network — it isn't published to the
host). `expose.host`, if set, generates a Traefik router-rule label
(`Host(...)`,) routing that hostname to this service; `expose.entrypoint`,
if non-empty, restricts that router to the named Traefik entry points
instead of all of them.

`expose.host` is what switches Traefik routing on at all. With no `host`
set there's no router, so neither `entrypoint` nor `middleware` produces
a label — they're silently dropped rather than emitted against a router
that doesn't exist.

Because `host` is spliced directly into the router rule
(``Host(`...`)``, which has no escape for its own backtick delimiter),
`hllc` rejects a `host` containing any rule metacharacter — a backtick
above all, plus `` ( ) { } | & , " ' \ ``. Each `entrypoint` entry is
checked against that same set, comma included: `hllc` owns the comma
that joins entry points, so a comma inside one name would splice an
extra entry into the label. (`entrypoint "web,web-secure"` is therefore
an error — write `entrypoint web, web-secure`.) A comma is rejected in a
`middleware` name for the same reason.

## `volume`

Map-kind. Bare-entry separator: `->` (host path/volume name → container
path). Uniqueness is checked on the **container path** (the value side)
— Docker itself refuses two mounts at the same container path, but
allows the same host path mounted more than once.

```hll
volume syncthing-config {}

service syncthing {
  image "lscr.io/linuxserver/syncthing:latest"
  volume "/mnt/media" -> "/data"         # bind mount
  volume "syncthing-config" -> "/config" # named volume
}
```

Repeating `volume` accumulates entries rather than overwriting.

A host side starting with neither `/` nor `.` is a **named Docker
volume**; anything else is a bind mount, so `./jellyfin` and `../shared`
are bind mounts just as `/mnt/media` is, not only absolute paths.

A named volume must have a matching top-level `volume` declaration
somewhere in the same file, exactly as a `networks [x]` entry must have
a matching top-level `network` declaration. Referencing one that isn't
declared is a compile error:

```text
syncthing.hll:6:10: service `syncthing` references undeclared volume `snycthing-config`
```

That's what catches a typo or an accidental collision: before the
declaration was required, `snycthing-config` simply became a second,
empty volume, and two services that happened to write the same string
were indistinguishable from two services deliberately sharing one. Now
sharing is stated explicitly — both services reference the one
declaration:

```hll
volume shared-media {}

service jellyfin {
  image "jellyfin/jellyfin:latest"
  volume "shared-media" -> "/data"
}

service sonarr {
  image "lscr.io/linuxserver/sonarr:latest"
  volume "shared-media" -> "/media"
}
```

Bind mounts need no declaration at all — they name a host path, not
something Docker manages, and Docker itself requires no pre-declaration
for one.

Each *referenced* named volume becomes an entry in the Compose
document's top-level `volumes:` section, carrying whatever `external`/
`name`/`driver`/`driver_opts` its declaration set. A declared but never
mounted volume isn't emitted, the same way an unreferenced `network`
declaration isn't.

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
  router. However many you list, they produce **one** label, not one per
  item: `traefik.http.routers.<service>.middlewares=` with the names
  comma-joined. Every name also gets an `@file` suffix appended
  (`middlewares=local-ipwhitelist@file,forwardAuth-authentik@file`) —
  that's Traefik's file-provider reference convention, applied
  unconditionally, so write the bare middleware name and let `hllc` add
  it. Like `expose`'s `entrypoint` — which joins its own list the same
  way, just without the `@file` suffix — `middleware` generates nothing at
  all unless `expose.host` is set: with no host there's no router to
  attach anything to.
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
| string | not set — Compose's own per-project name applies |

Only emitted when set explicitly. Compose's own default container
naming is scoped per project and is what most people want; an explicit
`container_name` forces one specific name everywhere it's deployed, so
it's an opt-in override (for a stable DNS name or an external
reference), not something every service should get by default —
defaulting it to the service's own name reliably collides across
independent stacks that happen to share a service name (`db`, `broker`,
...), and Compose refuses to start the second container with the same
name.

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

A `raw` value's lists and maps may nest up to 128 levels deep; past that
`hllc` reports an error rather than following the nesting further. Real
Compose structures nest a handful of levels, so this only ever comes up
for generated or pathological input.

### `raw` wins over a built-in field of the same name

A `raw` key may name a field `hll` already has (`image`,
`container_name`, `restart`, `environment`, `volumes`, `networks`,
`dns`, `expose`, `depends_on`, `labels`). When it does, the `raw` value
is what's emitted and the built-in one is dropped — the key appears
exactly once:

```hll,fragment
image "nginx"
raw {
  image: "nginx:1.27-alpine"   # this is the image that's emitted
}
```

This is what makes `raw` a durable escape hatch. A file that writes
`raw { ports: [...] }` today, because there's no dedicated `ports`
field yet, keeps working unchanged the day one is added — so gaining a
built-in field is never a breaking change for files that were working
around its absence.

Note that `raw`'s value **replaces** the built-in one; it never merges
with it. That's worth knowing for `labels` in particular, since `hll`
computes the Traefik labels itself:

```hll,fragment
expose 8080 as "web.example.com"
raw {
  labels: ["only.this=1"]   # every computed Traefik label is dropped
}
```

Overriding a service's `volumes:` or `networks:` key doesn't retract
the top-level `volumes:`/`networks:` declarations that `volume` and
`networks` produced — those stay, so a `raw` replacement naming the
same named volume or network still resolves.

## `with`

Not really a "field" you set directly so much as the mechanism for
pulling a `template`'s fields onto a `service` — see [Templates &
Composition](./templates-and-composition.md) for `with` in full.
