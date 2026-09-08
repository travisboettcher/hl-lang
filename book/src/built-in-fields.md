# Built-in fields

This is a reference for every field `hll` understands: what it accepts,
its default, and what it produces in the generated Compose YAML. See
[Syntax Basics](./syntax-basics.md) for the shorthand forms referenced
below—primary-value, secondary-field, map-style.

## `service`, `network`, and `volume`

`service`, `network`, and `volume` are the three top-level declaration
types—each requires a name, such as `service jellyfin { ... }`, `network
traefik-net { ... }`, or `volume syncthing-config { ... }`, and a body. A
`template` body accepts exactly the same set of fields as `service`—see
[Templates & Composition](./templates-and-composition.md).

`volume` names two different things, depending on where you write it. At
the top level it *declares* a named Docker volume, which this section
covers. Inside a `service` or `template` body it *mounts* one, the
map-kind [`volume` field](#volume) further down.

### `network` fields

| Field | Accepts | Default |
|---|---|---|
| `external` | bare flag, no value | unset, `false` |
| `name` | string | the network's own `hll` identifier |

`external` marks a network as one Docker already manages (for example, one
`docker compose` created for another stack) rather than one this file's
own Compose output should create. `name` is the real underlying Docker
network name, when it differs from the identifier you declared the
network under—needed because Compose's own auto-derived network names
depend on the directory you ran `docker compose` from, which the
compiler can't know:

```hll
network traefik-net {
  external
  name: "docker_default"
}
```

Whatever `name` ends up as, `traefik-net.name` reads it back from any
value position—a label that has to carry the real Docker name, say. See
[Reading a declaration's real
name](./templates-and-composition.md#reading-a-declarations-real-name).

### `volume` declaration fields

| Field | Accepts | Default |
|---|---|---|
| `external` | bare flag, no value | unset, `false` |
| `name` | string | the volume's own `hll` identifier |
| `driver` | string | unset, matching Compose's own default of `local` |
| `driver_opts` | map body, `key: value` | empty |

`external` and `name` mean exactly what they mean on a `network`, down
to `media.name` reading the second one back. The first marks a volume
Docker already manages rather than one this file's own Compose output
should create. The second is the real underlying Docker volume name,
when it differs from the identifier you declared the volume under. `hllc` passes `driver` and `driver_opts` straight through
to Compose:

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

Every named volume a service mounts needs one of these declarations—see
the [`volume` field](#volume) for what counts as a named volume and why
`hllc` requires the declaration.

## `image`

Primary field: `ref`.

| Field | Accepts | Default |
|---|---|---|
| `ref` | string | *required—no default* |

```hll,fragment
image "jellyfin/jellyfin:latest"
```

Every service needs an `image` or a [`build`](#build)—either directly or
inherited from a template—and `hllc build` fails when it would emit
neither.

That check reads the *generated document*, not the `image` field, so a
[`raw`](#raw) entry supplying the key by hand counts:

```hll,build
service foo {
  raw {
    image: "test:latest"
  }
}
```

## `build`

Primary field: `context`.

| Field | Accepts | Default |
|---|---|---|
| `context` | string | *Required—a build with no context has nothing to build* |
| `dockerfile` | string | unset—Compose looks for `Dockerfile` inside the context |

Compose's own `build:` key, for a service built from a local Dockerfile
rather than pulled from a registry:

```hll,build
service vault-git-sync {
  build "./vault-git-sync"
  restart unless-stopped
}
```

```text
services:
  vault-git-sync:
    build: ./vault-git-sync
    restart: unless-stopped
```

Name a `dockerfile` and `hllc` switches to Compose's long form, since
the short one has nowhere to put it. `{{name}}` resolves in both halves,
the same as in `image`:

```hll,build
service app {
  build {
    context: "./{{name}}"
    dockerfile: "Dockerfile.prod"
  }
}
```

```text
services:
  app:
    build:
      context: ./app
      dockerfile: Dockerfile.prod
```

`image` and `build` are independent—set either, or both. Compose reads
the pair as *build this context, then tag the result as that image.*

`build` deliberately has no `args`. It's a map, unlike the two plain
strings here, and nothing has needed one yet. `raw { build: { ... } }`
overrides the whole key for a service that does.

## `expose`

Primary field: `port`—the one field this type has.

| Field | Accepts | Default |
|---|---|---|
| `port` | number | *No default—omitting `expose` entirely just means Compose gets no `expose:` entry* |

```hll,fragment
expose 8096
```

`expose` is Compose's own `expose:` key—container-network visibility,
reachable from other containers on the same network but never published
to the host (for that, see [`publish`](#publish)). It has nothing to do
with which hostname reaches the service.

### Routing isn't a built-in field

`expose` says the port is reachable inside the Compose network. It says
nothing about which hostname reaches it, which is a reverse proxy's
question rather than Compose's, and `hllc` no longer answers it: there
is no `router` field, no `traefik` field, and no `expose <port> as
"<host>"` sugar. Routing goes in [`labels`](#labels), and the templates
in [`std:traefik`](./routing.md) write those labels for you.

The load-balancer port label goes with it—a `port` template writes it
when a router needs one. What survives here is `expose` itself, doing
the one job Compose gives it.


## `publish`

Map-kind. Bare-entry separator: `->`, which points from the host port to
the container port. `hllc` checks uniqueness on the **container port**,
the value side, the same convention `volume` follows for its own
`host -> container` mapping.

`publish` is Compose's `ports:` key, which puts the port on the Docker
host where the rest of the local network can reach it. That's the
opposite of [`expose`](#expose), Compose's `expose:` key, which reaches
only other containers on the same network. A service behind a reverse
proxy wants `expose`, plus the labels that route to it (see
[Routing](./routing.md)). A service that takes traffic directly,
such as Pi-hole on 53, Syncthing's sync port, or a game server, wants
`publish`. Setting both is fine and means both things.

```hll,build
service pihole {
  image "pihole/pihole:latest"
  publish 53 -> "53/tcp"
  publish 53 -> "53/udp"
  publish 8081 -> 80
  restart unless-stopped
}
```

```yaml
services:
  pihole:
    image: pihole/pihole:latest
    restart: unless-stopped
    ports:
      - "53:53/tcp"
      - "53:53/udp"
      - "8081:80"
```

Repeating `publish` accumulates entries rather than overwriting. A
service with several published ports, such as Jellyfin's 8096 and 8920
or Syncthing's 8384 and 22000, gets one line each.

Write both sides exactly as you'd write them in Compose's short syntax.
`hllc` passes both through to the generated `host:container` string
unchanged. A protocol suffix belongs on the container side, quoted so it
lexes as one value: `publish 53 -> "53/udp"` yields `"53:53/udp"`.
Quoting the host side works the same way when you need to pin an
interface, as in `publish "127.0.0.1:8081" -> 80`.

Checking uniqueness on the container side rather than the host one is
deliberate. Docker itself conflicts on the host port, but the protocol
suffix rides on the container half of the mapping, so a host-side check
would reject the legal pair in the preceding example. The trade-off is
the mirror image: `hllc` rejects one container port published on two
different host ports, `8080 -> 80` *and* `8081 -> 80`, as a duplicate.
Reach for [`raw`](#raw)'s `ports:` when you genuinely need that.

There's no single-value shorthand. `publish 8096` is an error, not
"8096 on both sides." `volume` requires both sides of its mapping too,
and both fields follow the same rule.

## `devices`

Map-kind. Bare-entry separator: `->`, which points from the host device
path to the container device path. `hllc` checks uniqueness on the
**container path**, the value side—the same convention `publish` follows
for its own `host -> container` mapping, and for the same reason.

```hll,build
service cadvisor {
  image "gcr.io/cadvisor/cadvisor:latest"
  devices "/dev/kmsg" -> "/dev/kmsg"
  privileged
}
```

```yaml
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    privileged: true
    devices:
      - /dev/kmsg:/dev/kmsg
```

`devices` is Compose's own `devices:` key, exposing a host device inside
the container—`cadvisor`'s classic use case, reading host `/proc` and
control-group device metrics. It's a plain generic Compose key like `dns`, not
homelab-specific itself even though a real entry always is. `hllc`
never validates or rewrites a device path—write it exactly as `docker
compose` would expect it.

Repeating `devices` accumulates entries rather than overwriting, exactly
like `publish`.

Write both sides exactly as you'd write them in Compose's short syntax,
`HOST:CONTAINER[:CGROUP_PERMISSIONS]`. `hllc` passes both through to the
generated `host:container` string unchanged. An optional control-group
permissions suffix belongs on the container side, quoted so it lexes as
one value: `devices "/dev/sda" -> "/dev/xvda:rwm"` yields
`"/dev/sda:/dev/xvda:rwm"`.

Checking uniqueness on the container side rather than the host one is
deliberate, and the reasoning transfers directly from `publish`—Docker's
real conflict is on the host side, but the optional permissions suffix
rides on the container half of the mapping, so a host-side check would
reject the legitimate case of one host device mapped to two container
paths, each with its own permissions. The trade-off is the mirror image:
`hllc` rejects one container path fed by two different host devices,
`"/dev/sda" -> "/dev/xvda"` *and* `"/dev/sdb" -> "/dev/xvda"`, as a
duplicate. Reach for [`raw`](#raw)'s `devices:` when you genuinely need
that.

There's no single-value shorthand, matching `publish`/`volume`:
`devices "/dev/kmsg"` is an error, not "the same path on both sides."

## `volume`

Map-kind. Bare-entry separator: `->`, which points from the host path
or volume name to the container path. `hllc` checks uniqueness on the
**container path**, the value side—Docker itself refuses two mounts at
the same container path but allows the same host path mounted more
than once.

```hll
volume syncthing-config {}

service syncthing {
  image "lscr.io/linuxserver/syncthing:latest"
  volume "/mnt/media" -> "/data"       # bind mount
  volume syncthing-config -> "/config" # named volume
}
```

Repeating `volume` accumulates entries rather than overwriting.

The two entries in that example differ in one visible way, and it's the
only thing `hllc` goes by: **quoting**. A quoted host side is a path on
the machine Compose runs on, whatever the path looks like. An unquoted
one is an identifier naming a **named Docker volume**, exactly as an
entry in a `networks [x]` list names a network. So `volume "media" ->
"/data"` mounts a host path called `media`, while `volume media ->
"/data"` mounts the volume declared as `volume media { ... }`.

Only the unquoted form takes an `alias.name` qualifier, since only a
reference names something an `.hll` file declares. See
[Imports](./imports.md) for importing a volume across files.

Every named volume needs a matching top-level `volume` declaration—in
the file that mounts it, or in a file it imports—exactly as a
`networks [x]` entry needs a matching top-level `network` declaration.
Reference one you never declared and `hllc` reports a compile error:

```text
syncthing.hll:6:10: service `syncthing` references undeclared volume `snycthing-config`
```

That error catches a typo or an accidental collision. Before `hllc`
asked for the declaration, `snycthing-config` quietly became a second,
empty volume, and two services that happened to write the same string
looked exactly like two services deliberately sharing one. Now each file
states the sharing outright, with both services naming the one
declaration—and a misspelling has nothing to resolve to:

```hll
volume shared-media {}

service jellyfin {
  image "jellyfin/jellyfin:latest"
  volume shared-media -> "/data"
}

service sonarr {
  image "lscr.io/linuxserver/sonarr:latest"
  volume shared-media -> "/media"
}
```

Bind mounts need no declaration at all. They name a host path rather
than something Docker manages, and Docker itself asks for no
pre-declaration either. `hllc` passes a quoted host side through to
Compose as written, so `./jellyfin`, `../shared`, and `/mnt/media` all
behave the way Compose's own short syntax says they do.

`hllc` gives every *referenced* named volume an entry in the Compose
document's top-level `volumes:` section, carrying whatever `external`,
`name`, `driver`, and `driver_opts` its declaration set. A volume you
declare but never mount produces no entry, exactly as a `network`
declaration no service names produces none—though only the `network`
case raises a [warning](./cli.md#warnings) today.

Either kind of entry may add a trailing `{ read_only }` body, appending
Compose short syntax's `:ro` mode suffix:

```hll,fragment
volume "/" -> "/rootfs" { read_only }
volume media -> "/data" { read_only }
```

produces:

```yaml,fragment
volumes:
  - /:/rootfs:ro
  - media:/data:ro
```

`read_only` is bare presence only, matching `network`'s own `external`
flag—there's no `read_only: true`/`read_only: false` form, and no way to
write `:rw` explicitly, since that's already Compose's own default for
an entry with no mode suffix at all. It works the same way whether the
host side is a bind-mount path or a named-volume reference, and inside
`volume`'s canonical multi-entry body a flagged entry and an unflagged
one can sit side by side:

```hll,fragment
volume {
  "/" -> "/rootfs" { read_only }
  "/data" -> "/data"
}
```

`hll` covers only `:ro` today, not Compose's other short-syntax mount
options (`:z`, `:Z`, tmpfs sizing)—see the design doc's `volume` section
for why this design picked a bare flag over a general `mode` string.

## `env`

Map-kind. Bare-entry separator: `=`, key equals value. `hllc` checks
uniqueness on the **key**—two `env` entries can't set the same
variable.

```hll,fragment
env PUID = "1000"
env PGID = "100"
```

Repeating `env` accumulates entries.

## `labels`

Map-kind. Bare-entry separator: `:`, so the short form and the canonical
form are the same thing. `hllc` checks uniqueness on the **key**, like
`env`.

`labels` holds the service's Docker labels. Entries arrive from every
template the service applies as well as from its own body, and `hllc`
**adds** them rather than letting one set replace another, which is what
lets templates carry a service's routing and still leaves room for a
label of your own:

```hll,build
use "std:traefik" as traefik

network traefik-net {
  external
  name: "docker_default"
}

service web {
  image "nginx"
  networks [traefik-net]

  with
    traefik.http { host: "web.example.com", port: 8123 },
    traefik.http_entrypoints { router: "{{name}}", entrypoints: ["web-secure"] }

  labels {
    "traefik.http.routers.web.tls.domains[0].main": "internal.example.com"
    "com.example.owner": "platform-team"
  }
}
```

```yaml
labels:
- traefik.http.routers.web.rule=Host(`web.example.com`)
- traefik.http.services.web.loadbalancer.server.port=8123
- traefik.http.routers.web.entrypoints=web-secure
- traefik.http.routers.web.tls.domains[0].main=internal.example.com
- com.example.owner=platform-team
```

Entries land in tier order: each `with` target left to right, then the
service's own body last. Nothing a template wrote moves to make room, so
a service that applies no template emits exactly what it wrote.

This is where routing labels go, either written by hand or by the
templates in [`std:traefik`](./routing.md), and where a label no
template covers goes—the standard example is a per-router list of TLS
Subject Alternative Name (SAN) domains. It's also what
[`raw { labels: ... }`](#raw) can't do: `raw` replaces the whole list,
so one extra line costs you every other label unless you retype them
all.

### Quoting

Traefik's label keys carry dots and brackets, and neither can appear in a
bare word, so a key like
`traefik.http.routers.web.tls.domains[0].main` needs quotes. That's the
one ergonomic cost of the map shape, and it buys the duplicate-key check
below—which a list of `"key=value"` strings, closer though it reads to
Traefik's own documentation, has no way to perform.

Docker reads a label as `key=value`, splitting at the first `=`, so a key
containing one would name a different label than the one written. `hllc`
rejects that rather than emitting it.

### A value goes through as written

`hllc` checks the key and leaves the value alone. That's deliberate. A
Traefik rule is mostly backticks, parentheses and `||`, so a guard
strict enough to be worth having would reject the labels this field
exists to write:

```hll,build
service web {
  image "nginx"
  labels {
    "traefik.http.routers.web.rule": "Host(`web.example.com`) || Host(`www.example.com`)"
  }
}
```

```yaml
labels:
- traefik.http.routers.web.rule=Host(`web.example.com`) || Host(`www.example.com`)
```

The cost is that nothing checks what the value *means*. `hllc` once
parsed rule syntax and could see that a stray backtick closed a `Host(`
call early, so it refused a host containing one:

```text
4:11: `router.host` must not contain '`' — it would change the meaning of the generated Traefik label
```

That check went with the grammar. Routing is a string now, so the same
value compiles and reaches Traefik intact—as a rule matching every host
rather than one:

```hll,fragment
labels {
  "traefik.http.routers.web.rule": "Host(`ok.example.com`) || HostRegexp(`{any:.+}`)"
}
```

So a value you assemble from somewhere else—a template parameter, most
of all—is yours to vet. The check `hllc` drops here applies at compile
time to text sitting in your own `.hll` source, which is what makes
leaving it out defensible: a bad value breaks your own homelab rather
than opening it to a stranger.

### A key written twice is an error

Two entries claiming one key is a compile error naming both, exactly as
for `env`:

```hll,ignore
labels {
  "com.example.owner": "platform-team"
  "com.example.owner": "someone-else"
}
```

```text
5:5: duplicate `labels` entry: key "com.example.owner" already set at 4:5
```

### A key two entries resolve to is an error

The preceding check compares keys as written, which isn't the same as
comparing the keys that reach Compose: `{{name}}` resolves later, so two
entries spelled differently in source can still land on one key. `hllc`
catches that too, and the message names both sides:

```hll,ignore
service web {
  image "nginx"
  expose 8123
  labels {
    "traefik.http.routers.{{name}}.rule": "Host(`web.example.com`)"
    "traefik.http.routers.web.rule": "Host(`elsewhere.example.com`)"
  }
}
```

```text
6:5: label "traefik.http.routers.web.rule" is already set by this service's own `labels` at 5:5 — two entries spelled differently can still resolve to one key once `{{name}}` is substituted, so one of them would silently do nothing
```

Neither spelling wins, deliberately. Whichever one lost would be a line
you wrote that quietly does nothing. Refusing is the only outcome where
every line either takes effect or gets a diagnostic.

### Merging across templates

A single-valued entry merges exactly the way `env` does. A template's
entries reach the service, the service's own body wins over a template
that set the same key, and two `with`-listed templates setting one key
is a `MapKeyCollision`—see [Templates &
Composition](./templates-and-composition.md).

`{{name}}` resolves in both halves of an entry, so
`"com.example.{{name}}.owner": "{{name}}-team"` works the way it does
under `env`.

### A list value composes instead of colliding

Write a bracketed list and the entry means something different when two
places set it: the values join rather than conflict.

```hll,build
template internal_web(port) {
  expose $port
  labels { "traefik.http.routers.{{name}}.middlewares": ["local-ipwhitelist@file"] }
}

template authenticated {
  labels { "traefik.http.routers.{{name}}.middlewares": ["forwardAuth-authentik@file"] }
}

service syncthing {
  image "lscr.io/linuxserver/syncthing"
  with internal_web { port: 8384 }, authenticated
}
```

```yaml
labels:
- traefik.http.routers.syncthing.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file
```

The list renders comma-joined, the same separator a
[list argument](./templates-and-composition.md#passing-a-list)
interpolates with. Entries dedupe, so naming one twice across two
templates gets you one. Your service body adds to what its templates
supplied rather than replacing it.

That difference between the two shapes is the point of having both. A
single value says the key holds one thing, so two templates setting it
are two answers to one question and the collision is right. A list says
the key holds several, so several places contributing is the whole idea.
Writing one key as a list in one place and a single value in another is
an error—the two disagree about which kind of thing the key holds:

```text
10:12: `labels` key "com.example.tags" is a single value here but a list at 6:12 — a list composes across templates and a single value doesn't, so the two say different things about what this key holds
```

## `restart`

Primary field: `policy`.

| Field | Accepts | Default |
|---|---|---|
| `policy` | bare word or string | unset, matching Compose's own default of no automatic restart |

```hll,fragment
restart unless-stopped
```

Writing `image` or `restart` more than once in the same body is a
compile error, since both are scalar fields, not repeatable—unlike
`volume`/`publish`/`env`/`middleware`/`depends_on`.

## `healthcheck`

No primary field—unlike `image`'s `ref` or `expose`'s own `port`, no one
sub-field stands in for the whole healthcheck, so `healthcheck { ... }`
requires the braced body. `healthcheck "..."` doesn't parse.

| Field | Accepts | Default |
|---|---|---|
| `test` | string or bracketed list | unset—no healthcheck defined here, though the image's own still applies if it has one |
| `interval` | string | unset, matching Compose's own default |
| `timeout` | string | unset, matching Compose's own default |
| `retries` | number | unset, matching Compose's own default |
| `start_period` | string | unset, matching Compose's own default |
| `start_interval` | string | unset, matching Compose's own default |
| `disable` | bare flag, no value | unset, `false` |

```hll,fragment
healthcheck {
  test: "pg_isready -U miniflux"
  interval: "10s"
  timeout: "5s"
  retries: 3
  start_period: "30s"
  start_interval: "5s"
}
```

`test` accepts either a bare string—Compose's shell form, run through the
container's own shell (a bare string is shorthand for `CMD-SHELL
<string>`)—or a bracketed list—Compose's exec form, run directly with no
shell involved. `hllc` carries whichever form you write straight through
to the generated `test:` key, rather than normalizing one into the
other:

```hll,build
service miniflux-db {
  image "postgres:15"
  healthcheck {
    test: ["CMD", "pg_isready", "-U", "miniflux"]
    interval: "10s"
    start_period: "30s"
  }
}
```

```yaml
services:
  miniflux-db:
    image: postgres:15
    healthcheck:
      test:
        - CMD
        - pg_isready
        - -U
        - miniflux
      interval: 10s
      start_period: 30s
```

`hllc` carries `interval`/`timeout`/`start_period`/`start_interval`/
`retries` through exactly as written—it doesn't parse or validate
Compose's duration syntax (`"10s"`, `"1m30s"`) or check that `retries`
is a sane, non-negative count. A mistake there is `docker compose
config`'s to catch, not `hllc`'s.

`disable` sets Compose's own `disable: true`, which turns the
healthcheck off entirely—including one the image itself defines:

```hll,fragment
healthcheck {
  disable
}
```

Writing `healthcheck` more than once in the same body is a compile
error, same as `image`/`restart`/`expose`—it's a struct-kind field, not
repeatable.

## `depends_on`, `networks`, `dns`, `env_file`

All four are plain list fields directly on `service`/`template`, not
nested struct types, so there's no primary-field shorthand to learn for
them. Write a bare identifier or string, a bracketed list, or repeat the
field:

```hll,fragment
depends_on database
depends_on cache                   # repeating accumulates
depends_on [database { condition: service_healthy }]

networks [traefik-net]

dns ["192.168.50.182"]

env_file "miniflux.env"
env_file ["miniflux.env", "common.env"]
```

A middleware list looks like it belongs to this group but isn't a field
at all: a middleware reaches Traefik as a label on one specific router,
so it's written in [`labels`](#labels)—see [Routing](./routing.md).

- `depends_on` names a same-file sibling `service` this one depends
  on—it's not cross-file, and doesn't accept a qualified `alias.name`.
  Each entry may optionally add a `{ condition: ... }` body naming one
  of Compose's own three readiness conditions—`service_started` (the
  default: wait only for the target container to start, which is all a
  bare `depends_on database` has ever meant), `service_healthy` (wait
  for the target's `healthcheck` to report healthy), or
  `service_completed_successfully` (wait for the target to exit
  zero—typically a one-shot init/migration container). Anything else is
  a compile error naming all three. A bare entry and a conditioned one
  can sit side by side in the same list:

  ```hll,fragment
  depends_on [cache, database { condition: service_healthy }]
  ```

  Compose has two mutually exclusive shapes for `depends_on:` and never
  mixes them in one document: a plain list of names, or a mapping of
  name to `{ condition: ... }`. `hllc` emits the plain list as long as
  *no* entry in the field carries a condition, and switches the whole
  field to the mapping form once *any* entry does. A sibling entry with
  no explicit condition is then filled in with `service_started`, since
  the mapping form requires every entry to name one.

  `service_healthy` is only meaningful when the target service actually
  has a healthcheck to become healthy against—but `hllc` doesn't warn
  when the target's `.hll` body has no [`healthcheck`](#healthcheck)
  field, because that's not evidence the condition is meaningless: a
  Docker image can bake its own `HEALTHCHECK` into its Dockerfile,
  invisible to anything an `.hll` file declares.
- `networks` references a top-level `network` declared in the same
  program—see the preceding section. If exactly one referenced network
  is `external`, its real name also drives the
  `traefik.docker.network=` label, but more than one `external` network
  on the same service is a compile error, since it's ambiguous which
  network Traefik should target. `hllc` builds the generated `networks:`
  section from these references, so a `network` no service names never
  reaches the output. That one is a warning on stderr rather than an
  error—see [Warnings](./cli.md#warnings).

  `default` is the one network name every program gets for free, with or
  without a matching declaration: `networks [default]` compiles even
  when nothing in the file declares `network default { ... }`, resolving
  to the same implicit default network `docker compose` itself creates
  for a project. `hllc` adds nothing to the top-level `networks:`
  section for it in that case—Compose already knows about `default`, so
  there's nothing for `hllc` to declare.

  Two or more `service` declarations in one file are, by construction,
  one Compose stack meant to talk to each other, so every service in
  such a file is implicitly attached to `default` in addition to
  whatever it names explicitly—no `networks [default]` required. A
  single-service file gets no such auto-attachment. Compose's own
  implicit default network already covers a lone service for free, so
  there's nothing for `hllc` to add. Auto-attachment is idempotent—a
  service that writes `networks [default]` itself still ends up with one
  `default` entry, not two—and, when explicit, always sorts last in that
  service's `networks:` list.

  Attaching every service unconditionally is where `hllc` parts company
  with `docker compose` itself, which hands a service the default
  network only when that service lists no networks of its own. The
  difference is deliberate: `default` already carries the traffic
  between the services in one file, so a stack can lean on it and
  declare no private network for that job—one declaration fewer than the
  hand-written Compose it replaces, rather than a shortfall next to it.

  An explicit `network default { ... }` declaration still wins: its
  `external`/`name` settings apply exactly as they would to any other
  named network, including feeding the `traefik.docker.network=` label
  when it's `external`, and it still emits its own top-level `networks:`
  entry. The implicit, undeclared `default` is only a fallback for when
  no such declaration exists.
- `dns` sets Compose's own per-service `dns:` key—a resolver override.
  Use it, for example, when a network has a local name server.
- `env_file` sets Compose's own `env_file:` key—one or more paths to
  load environment variables from. It's a plain generic Compose key like
  `dns`, not homelab-specific itself, even though most real entries
  point at a gitignored, per-homelab `.env` file. The generated
  `env_file:` value is always a list: a single `env_file "one.env"`
  still emits a one-element `env_file:` list, so the generated shape
  doesn't depend on how many paths you wrote. Compose itself resolves
  each path relative to the Compose file, not `hllc`—write it exactly
  as `docker compose` would expect it. When two files set the same
  variable, Compose lets
  the later file win, so order matters here the same way it matters for
  `dns`'s resolver priority. Reach for [`env`](#env) instead when a
  value belongs directly in the `.hll` file rather than in an external
  file.

All five accumulate across repeated writes within one body. Across
template composition (see [Templates &
Composition](./templates-and-composition.md)), `middleware`/`networks`/
`dns`/`env_file` also just accumulate—there's no collision to check
since list fields can only ever grow. `depends_on` merges keyed on the
service name instead, and the service's own body always wins over a
template's entry for the same dependency—but two `with`-listed templates
naming the same service is *not* automatically a compile error: as the
preceding discussion of `depends_on` covers, it's only one when their
`condition`s actually disagree, exactly like two templates setting the
same [`env`](#env) key to two different values would collide. Two
templates that both say `depends_on [database]`—or that spell out the
same condition on both—are giving the same answer twice, not two
different ones, so they still collapse to a single entry exactly as
they always have.

## `privileged`

Another plain generic Compose key, directly on `service`/`template`:

```hll,fragment
privileged
```

| Field | Accepts | Default |
|---|---|---|
| `privileged` | bare flag, no value | unset, `false` |

`privileged` gives the container extended host privileges—Compose's own
`privileged:` key. Bare-presence only, matching `network`'s own
`external` field: there's no `privileged: false` form to write, since
absence already means false.

`cadvisor` is the service that motivated this field and
[`devices`](#devices) together: it needs `privileged` and a `devices`
mount to read host `/proc`/cgroups, previously written through
[`raw`](#raw) before these two fields existed.

## `container_name`

A plain scalar field directly on `service`/`template`, not a nested
struct type:

```hll,fragment
container_name "uptime-kuma"
```

| Accepts | Default |
|---|---|
| string | not set—Compose's own per-project name applies |

Only emitted when set explicitly. Compose's own default container
naming, scoped per project, is what most people want. An explicit
`container_name` forces one specific name everywhere it's deployed, so
it's an opt-in override you use for a stable hostname or an external
reference, not something every service should get by default.
Defaulting it to the service's own name reliably collides across
independent stacks that happen to share a service name (`db`, `broker`,
and so on), and Compose refuses to start the second container with the
same name.

## `command`

A plain scalar-or-list field directly on `service`/`template`, not a
nested struct type, sharing its grammar with [`healthcheck`](#healthcheck)'s
`test` sub-field—a bare string, Compose's shell form, or a bracketed
list, Compose's exec form:

```hll,fragment
command "npm start"
```

```hll,fragment
command ["--housekeeping_interval=30s", "--docker_only=true"]
```

| Accepts | Default |
|---|---|
| string or bracketed list | unset—the image's own `CMD`/entrypoint applies |

`command` overrides the arguments Compose passes to the image's
entrypoint, exactly like Compose's own `command:` key. `hllc` carries
whichever form you write straight through to the generated `command:`
key, rather than normalizing one into the other—the same rule
[`healthcheck`](#healthcheck)'s `test` follows, and for the same
reason: the shell form runs through the container's own shell, while the
exec form runs directly with no shell involved, so the two aren't
interchangeable. A comma inside one quoted list item is data, not a list
separator:

```hll,build
service cadvisor {
  image "gcr.io/cadvisor/cadvisor:latest"
  command [
    "--housekeeping_interval=30s",
    "--docker_only=true",
    "--enable_metrics=cpu,memory,network"
  ]
}
```

```yaml
services:
  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    command:
      - --housekeeping_interval=30s
      - --docker_only=true
      - --enable_metrics=cpu,memory,network
```

Writing `command` more than once in the same body is a compile error,
same as `image`/`restart`/`container_name`—it's single-occurrence, not
repeatable.

## `entrypoint`

A plain scalar-or-list field directly on `service`/`template`, sharing
its grammar with the preceding [`command`](#command)—a bare string,
Compose's shell form, or a bracketed list, Compose's exec form:

```hll,fragment
entrypoint "/bin/sh -c 'do-a-thing'"
```

```hll,fragment
entrypoint ["/bin/sh", "-c", "do-a-thing"]
```

| Accepts | Default |
|---|---|
| string or bracketed list | unset—the image's own `ENTRYPOINT` applies |

`entrypoint` overrides the image's `ENTRYPOINT`, exactly like Compose's
own `entrypoint:` key. `hllc` carries whichever form you write straight
through to the generated `entrypoint:` key, rather than normalizing one
into the other—the same rule [`command`](#command) follows, and for the
same reason: the shell form runs through the container's own shell,
while the exec form runs directly with no shell involved.

### `entrypoint` and `command` are two different keys

They're separate Compose keys and they override separate halves of what
the image declares. `entrypoint` replaces the image's `ENTRYPOINT`, the
program the container runs. `command` replaces its `CMD`, the arguments
that program gets. Docker joins them: the container runs the
`entrypoint` with the `command` appended. Set either one, both, or
neither—setting one says nothing about the other:

```hll,build
service backup {
  image "alpine:3"
  entrypoint ["/usr/local/bin/backup.sh"]
  command "--target=/data"
}
```

```yaml
services:
  backup:
    image: alpine:3
    entrypoint:
      - /usr/local/bin/backup.sh
    command: --target=/data
```

Writing `entrypoint` more than once in the same body is a compile error,
same as `command`—it's single-occurrence, not repeatable. And a service
that overrides `entrypoint` from a `with`-listed template replaces the
inherited value outright rather than appending to it, since the value is
one whole argument vector.

## `raw`

Map-kind, schema-free: `hllc` accepts unknown keys as-is rather than
checking them against a fixed field list, and their values pass
straight through to the generated YAML. Its job is the long tail the
language doesn't model with a field of its own: real Compose keys that
come up rarely enough, or are specific enough to one deployment, that a
dedicated field isn't worth it. `cadvisor`'s `security_opt` is one:

```hll,fragment
raw {
  security_opt: ["seccomp=unconfined"]
}
```

Each `raw` entry becomes a sibling top-level key on the generated
Compose service block (`security_opt: [...]`), exactly as written—there's
no validation, so `docker compose` itself is the first thing to reject a
misspelled key or a value Compose doesn't understand.

A `raw` value's lists and maps may nest up to 128 levels deep. Past
that, `hllc` reports an error rather than following the nesting
further. Real Compose structures nest a handful of levels, so this only
ever comes up for generated or pathological input.

`hllc` checks uniqueness on the **key**, the same convention `env` uses:
two *explicit* `with`-listed templates setting the same `raw` key is a
compile error, not a silent override.

A key repeated *within one body* is a compile error too—one `raw { }`
block, or two in the same service, since two blocks accumulate into one
map:

```hll,ignore
raw { user: "1000", user: "2000" }
```

```text
3:23: duplicate `raw` entry: key "user" already set at 3:9
```

That's the same diagnostic a repeated `env` key raises, and it names both
occurrences so you can see which value you were about to lose.

### What counts as a duplicate

A duplicate key is a key repeated inside **one** mapping, which is what
it means in YAML too. `raw` values nest, so a `raw` body is a tree of
mappings rather than one flat list of keys, and `hllc` checks each
mapping on its own:

```hll,fragment
raw {
  logging: { driver: "json-file" }
  "x-backup": { driver: "restic" }
}
```

Both nested maps hold a `driver`, and that's fine—they're two separate
mappings, so neither one repeats anything. A nested map may likewise
reuse a key its enclosing mapping already uses. Only writing one key
twice in the same `{ }` is an error.

### `raw` wins over a built-in field of the same name

A `raw` key may name a field `hll` already has: `image`,
`container_name`, `command`, `entrypoint`, `privileged`, `restart`,
`healthcheck`, `environment`, `env_file`, `volumes`, `networks`, `dns`,
`devices`, `ports`, `expose`, `depends_on`, or `labels`. When it does, the `raw`
value is what's emitted, and `hllc` drops the built-in one—the key
appears exactly once:

```hll,fragment
image "nginx"
raw {
  image: "nginx:1.27-alpine"   # this is the image that's emitted
}
```

This is what makes `raw` a durable escape hatch, and it isn't
hypothetical. Files that wrote `raw { ports: [...] }` before
[`publish`](#publish) existed still compile to exactly the same output
now that it does, so gaining a built-in field is never a breaking change
for files that were working around its absence. The same holds for
whichever Compose key gets a field next, so reaching for `raw` today
costs nothing later.

Note that `raw`'s value **replaces** the built-in one. It never merges
with it.

### `labels` is an aggregate, and replacing it costs more than it looks

`labels` deserves its own warning label, because the key `hllc` emits
isn't one field from an author's point of view. Its entries arrive from
every template the service applies as well as from its own
[`labels`](#labels) block, and since templates carry a service's routing
([`std:traefik`](./routing.md)), that usually means all of the service's
routing is in there.

So `raw { labels: [...] }` doesn't replace one block. It replaces
everything every contributor produced, all at once:

```hll,fragment
raw {
  labels: ["only.this=1"]   # every other label is dropped
}
```

The routing rule, the entry points, the load-balancer port—every entry
any template or block wrote—vanishes from that service, leaving
`only.this=1` as the entire `labels:` list `hllc` emits. Overriding `labels`
therefore means writing every one of those lines by hand, and keeping
them in step with the `.hll` file from then on.

`hllc` says so rather than letting it happen quietly. A service that
has labels *and* names `labels` in its `raw` block gets a warning:

```text
8:5: warning: `raw { labels: ... }` replaces service `web`'s computed
labels rather than adding to them, so every entry its `labels` blocks
and the templates it applies would have produced is dropped — write the
extra labels in a `labels { ... }` block instead, or reproduce the ones
you still need in this list
```

It's a warning, not an error. Hand-writing the whole label list is a
legitimate thing to do—it's exactly what `raw` is for when a label needs
a shape `labels` can't yet write—so the build still succeeds and the
generated document stays exactly as it was.

For adding a label rather than replacing every one, reach for the
[`labels`](#labels) field instead. It adds to the set, checks for
duplicate keys, and refuses two entries that resolve to one key instead
of quietly dropping one. `raw { labels: ... }` stays the escape hatch
for the case where you really do want to write the entire list by
hand—and it overrides a `labels` field too, since it replaces the
emitted key rather than any one contributor to it.

A service with no labels at all—none of its own and none from a
template—has nothing for the raw list to replace, and says nothing.

Overriding a service's `volumes:` or `networks:` key doesn't retract
the top-level `volumes:`/`networks:` declarations that `volume` and
`networks` produced—those stay, so a `raw` replacement naming the
same named volume or network still resolves.

## `with`

Not really a "field" you set directly so much as the mechanism for
pulling a `template`'s fields onto a `service`—see [Templates &
Composition](./templates-and-composition.md) for `with` in full.
