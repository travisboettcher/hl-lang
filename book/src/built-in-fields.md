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

### `volume` declaration fields

| Field | Accepts | Default |
|---|---|---|
| `external` | bare flag, no value | unset, `false` |
| `name` | string | the volume's own `hll` identifier |
| `driver` | string | unset, matching Compose's own default of `local` |
| `driver_opts` | map body, `key: value` | empty |

`external` and `name` mean exactly what they mean on a `network`. The
first marks a volume Docker already manages rather than one this file's
own Compose output should create. The second is the real underlying
Docker volume name, when it differs from the identifier you declared the
volume under. `hllc` passes `driver` and `driver_opts` straight through
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

Every service needs an `image`—either directly or inherited from a
template—`hllc --build` fails if one is missing.

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
to the host (for that, see [`publish`](#publish))—plus the
`traefik.http.services.<service>.loadbalancer.server.port` label,
written whenever the service has at least one [`router`](#router). It has
nothing to do with which hostname routes to a service or which Traefik
entry points a router attaches to—that's `router`'s job entirely, one
section down.

### The `as` sugar

`expose <port> as "<host>"` desugars to `expose { port }` plus an
unnamed `router { host }`—it's sugar for the single-router service,
which is most of them:

```hll,fragment
expose 8096 as "media.example.com"
# same as:
# expose { port: 8096 }
# router { host: "media.example.com" }
```

`as` is a one-shot fusion, not a list—you can't follow it with more
fields. A service that also needs an entry point, a path prefix, or a
second router writes `router` out explicitly instead:

```hll,fragment
expose 8096
router {
  host: "media.example.com"
  entrypoint: web-secure
}
```

**Migrating from an older `hll` file:** `expose 80, host: "h",
entrypoint: web`—a pre-#198 spelling—becomes `expose 80` plus a
`router` block naming the same `host`/`entrypoint` pair.

## `router`

No primary field: the router's name already occupies the position right
after the keyword, so there's nowhere for a bare value to go.

| Field | Accepts | Default |
|---|---|---|
| `host` | string | *Required—a router with no host has no rule* |
| `entrypoint` | reference list | empty—label omitted, so Traefik attaches the router to every entry point |
| `path_prefix` | list of strings | empty—the rule matches the host alone |
| `middleware` | reference list | empty—this router takes the service-level [`middleware`](#middleware-depends_on-networks-dns-env_file) list instead |

A `router` block declares one Traefik router, and owns every field that
only ever means anything attached to one. Write it as many times as you
need, each block naming its own router:

```hll,fragment
router api {
  host: "vikunja.example.com"
  entrypoint: web-secure
  path_prefix: ["/api/v1", "/dav/", "/.well-known/"]
}
router frontend {
  host: "vikunja.example.com"
  entrypoint: web-secure
}
```

That produces four labels, two per router:

```text
traefik.http.routers.vikunja-api.rule=Host(`vikunja.example.com`) && (PathPrefix(`/api/v1`) || PathPrefix(`/dav/`) || PathPrefix(`/.well-known/`))
traefik.http.routers.vikunja-api.entrypoints=web-secure
traefik.http.routers.vikunja-frontend.rule=Host(`vikunja.example.com`)
traefik.http.routers.vikunja-frontend.entrypoints=web-secure
```

The name after the keyword becomes part of the label key:
`traefik.http.routers.<service>-<name>`. Leave it off (`router { ... }`,
no name) and the router takes the service's own name,
`traefik.http.routers.<service>`—the **unnamed** form, and the shape
`expose <port> as "<host>"` desugars to. Writing the unnamed form twice
in one body, by hand or once by hand and once through the `as` sugar,
is a compile error: two blocks can't both claim one router id.

A name is a bare identifier, so it holds letters, digits, `-`, and `_`
and nothing else. That restriction is deliberate rather than incidental:
the name lands in a label **key**, where a `.` would extend the key and
an `=` would end it, so a name that could hold either would let a router
write a `traefik.*` label other than the one you asked for.

The comma form works here too, the same shorthand a nested struct field
takes when it has more than one field:

```hll,fragment
router api, host: "vikunja.example.com", entrypoint: web-secure
```

`entrypoint` is a **reference list**, spelled exactly like `middleware`
below—a bare name, several comma-separated names, or a bracketed list.
However many you name, they produce **one** label:
`traefik.http.routers.<id>.entrypoints=` with the names comma-joined
(`entrypoints=web,web-secure`)—`hllc` writes the commas, you write the
names. Leave `entrypoint` off entirely and `hllc` emits no
`entrypoints=` label at all, which is Traefik's own way of saying
"attach this router to every entry point."

One caveat if you write a bare list in the preceding comma-shorthand
form: the list ends at the next `field:`, so `router api, entrypoint: web,
host: "vikunja.example.com"` sets one entry point and a host, not two
entry points. Put `entrypoint` last, use brackets (`entrypoint: [web,
web-secure]`), or use the braced `router { ... }` body if you want a
bare list in the middle.

`path_prefix` narrows the router to requests under one of the listed
paths. `hllc` joins them with `||` inside one parenthesized group and
`&&`s that onto the host match, which is what keeps `&&` from binding
tighter than `||` and matching every host under the last prefix. A single
prefix keeps its parentheses too, so the rule reads the same either way.

Because `hllc` splices `host` directly into the router rule
(``Host(`...`)``, which has no escape for its own backtick delimiter), it
rejects a `host` containing any rule metacharacter, most notably a
backtick, plus `` ( ) { } | & , " ' \ ``. `hllc` checks each `entrypoint`
entry against that same set, comma included: it owns the comma that
joins entry points, so a comma inside one name would splice an extra
entry into the label. (`entrypoint "web,web-secure"` is therefore an
error—write `entrypoint web, web-secure`.) `hllc` rejects a comma in a
`middleware` name for the same reason, and resolves `{{name}}` in `host`
too, so the `"{{name}}.internal.example.com"` template idiom works here.

All three reject a control character too, such as the newline a string
literal writes as `\n` (see
[Numbers and strings](./syntax-basics.md#numbers-and-strings)). A hostname,
an entry point name, and a middleware name have no use for one, and a
label carrying one no longer means what it reads as.

A `router` with no `host` is a compile error, not a router quietly
missing from the output:

```hll
service web {
  image "nginx"
  router api {
    entrypoint: web-secure
  }
}
```

```text
web.hll:3:3: service `web` declares `router api` with no `host`, so there is no rule for Traefik to match — add a host (`host: "web.example.com"`) or drop the `router`
```

Two `router` blocks with the same name are an error as well. That's not
two routers, it's one router described twice, with the second silently
winning—the same reason two `volume` entries can't share one container
path.

`router` deliberately doesn't carry a port:
`traefik.http.services.<service>.loadbalancer.server.port` is one label
per Compose service, not per router—a container listens on one port no
matter how many routers point at it—so it comes from
[`expose`](#expose)'s own `port` instead. A service with at least one
`router` block but no `expose <port>` is a compile error too: a router
with nothing to load-balance onto used to mean Traefik silently guessed
a port, and now means `hllc` refuses to compile.

```hll,ignore
service web {
  image "nginx"
  router api {
    host: "web.example.com"
  }
}
```

```text
web.hll:3:3: service `web` declares a `router` but sets no `expose <port>`, so Traefik has no port to load-balance onto — add `expose <port>` or drop the `router`
```

### Per-router `middleware`

A `router` block *does* carry its own `middleware` list, spelled exactly
like the service-level [`middleware`](#middleware-depends_on-networks-dns-env_file)
field below, and it **replaces** that list for this one router:

```hll,build
service gitea {
  image "gitea/gitea:latest"
  expose 3000
  router public {
    host: "git.example.com"
    entrypoint: web-secure
  }
  router internal {
    host: "git.internal.example.com"
    entrypoint: web-secure
    middleware: local-ipwhitelist
  }
}
```

```text
traefik.http.routers.gitea-public.rule=Host(`git.example.com`)
traefik.http.routers.gitea-public.entrypoints=web-secure
traefik.http.routers.gitea-internal.rule=Host(`git.internal.example.com`)
traefik.http.routers.gitea-internal.entrypoints=web-secure
traefik.http.routers.gitea-internal.middlewares=local-ipwhitelist@file
traefik.http.services.gitea.loadbalancer.server.port=3000
```

That's a public route and an internal, IP-restricted one off the same
container—the case a single service-wide list can't express, since it
would either put the allowlist on the public route or take it off the
internal one.

Leave `middleware` off a `router` block and that router takes the
service-level list, so nothing written before per-router middleware
existed changes:

```hll,build
service app {
  image "nginx"
  expose 80
  middleware forwardAuth-authentik
  router public, host: "app.example.com"
  router lan {
    host: "app.internal.example.com"
    middleware: local-ipwhitelist
  }
}
```

```text
traefik.http.routers.app-public.rule=Host(`app.example.com`)
traefik.http.routers.app-public.middlewares=forwardAuth-authentik@file
traefik.http.routers.app-lan.rule=Host(`app.internal.example.com`)
traefik.http.routers.app-lan.middlewares=local-ipwhitelist@file
traefik.http.services.app.loadbalancer.server.port=80
```

Because a router's own list replaces rather than extends, there's no way
to write *everything the service named, minus one.* To give one router
no middleware while a sibling carries some, move the list off the
service and onto the routers that want it—the same explicitness `router`
already asks for from `host`.

The service-level field still needs at least one router to attach
to—naming one with no `router` block anywhere is a compile error, not a
service quietly built without it:

```hll,ignore
service web {
  image "nginx"
  expose 80
  middleware forwardAuth-authentik
}
```

```text
web.hll:4:14: service `web` sets `middleware` but has no `router` to attach it to — add a router (`router { host: "web.example.com" }`) or drop the `middleware`
```

Earlier versions emitted no `labels:` key at all here and exited 0, so a
service whose author forgot a host deployed with its authentication
missing and nothing said so. A `router`'s own `middleware` needs no such
check: it can't exist without the block it's written inside.

## `traefik`

No primary field—like `healthcheck`, no one sub-field stands in for the
whole struct, so `traefik { ... }` requires the braced body.

`hllc` computes a Traefik label list for every service by default:
`traefik.docker.network=`, each [`router`](#router) block's rule and its
`entrypoints=`/`middlewares=` labels, and the load-balancer port.

The `disabled` flag switches all of that off for one service and emits
`traefik.enable=false` in its place, nothing else, not even
`traefik.docker.network=`, since Traefik's Docker provider never acts on
a service that turns it off, so that label would have nothing to do.

| Field | Accepts | Default |
|---|---|---|
| `disabled` | bare flag, no value | unset, Traefik labels computed normally |

```hll,build
service db {
  image "postgres:15"
  traefik {
    disabled
  }
}
```

```yaml
services:
  db:
    image: postgres:15
    labels:
      - "traefik.enable=false"
```

This is the dedicated answer to a shape several real homelab services
share: a backing database with no `router`/`middleware` of its own,
sitting next to a Traefik-facing sibling service in the same file.
Before this field, the only way to say "no Traefik" was to replace the
*entire* computed label list through `raw`:

```hll,fragment
raw {
  labels: ["traefik.enable=false"]
}
```

That still works—`raw`'s override rule doesn't change—but it's a blunt
instrument for a one-label change, and it silently stops tracking
whatever `traefik.docker.network=`/router labels a future edit to the
service would otherwise have added.

Only a `router` block and `middleware` conflict with `disabled`. Plain
`expose <port>` doesn't—it's Compose's own `expose:` key,
container-network visibility with no Traefik involvement at all, so a
service that turns Traefik off can still declare one:

```hll,build
service db {
  image "postgres:15"
  expose 5432
  traefik {
    disabled
  }
}
```

```yaml
services:
  db:
    image: postgres:15
    expose:
      - 5432
    labels:
      - "traefik.enable=false"
```

Setting a `router` block or `middleware` on a service that turns Traefik
off is a **compile error**, the same treatment
[`router`](#router)'s own router-less-middleware check gives the mirror
mistake:

```hll,ignore
service db {
  image "postgres:15"
  expose 5432 as "db.example.com"
  traefik {
    disabled
  }
}
```

```text
db.hll:3:15: service `db` sets `router`, but `traefik` is disabled (at db.hll:5:5), so there is no router for it to attach to — drop the `router` or remove `disabled`
```

Both sides of that contradiction mean something on their own—only the
pair together doesn't—so `hllc` refuses to guess which one the service
actually meant.

No brace-free `traefik disabled` spelling exists—`disabled` needs the
braced body, `traefik { disabled }`, exactly like
[`healthcheck`](#healthcheck)'s own `disable` flag.

## `publish`

Map-kind. Bare-entry separator: `->`, which points from the host port to
the container port. `hllc` checks uniqueness on the **container port**,
the value side, the same convention `volume` follows for its own
`host -> container` mapping.

`publish` is Compose's `ports:` key, which puts the port on the Docker
host where the rest of the local network can reach it. That's the
opposite of [`expose`](#expose), Compose's `expose:` key, which reaches
only other containers on the same network. A service behind Traefik
wants `expose` (plus a [`router`](#router) block, or its `as "<host>"`
sugar, for the routing itself). A service that takes traffic directly,
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

This field grew its arrow syntax from review feedback on the pull
request that introduced it at #167—it originally shipped taking a
single pre-joined `"host:container"` string, `devices
["/dev/kmsg:/dev/kmsg"]`, before landing on the same shape `publish`/
`volume` already use.

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
Compose short syntax's `:ro` mode suffix—see #158:

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

## `middleware`, `depends_on`, `networks`, `dns`, `env_file`

All five are plain list fields directly on `service`/`template`, not
nested struct types, so there's no primary-field shorthand to learn for
them. Write a bare identifier or string, a bracketed list, or repeat the
field:

```hll,fragment
middleware local-ipwhitelist
middleware forwardAuth-authentik   # repeating accumulates

depends_on database
depends_on [database { condition: service_healthy }]

networks [traefik-net]

dns ["192.168.50.182"]

env_file "miniflux.env"
env_file ["miniflux.env", "common.env"]
```

- `middleware` names a Traefik middleware to attach to this service's
  routers. However many you list, they produce **one** label per router,
  not one per item: `traefik.http.routers.<router>.middlewares=` with the
  names comma-joined. Every name also gets an `@file` suffix appended
  (`middlewares=local-ipwhitelist@file,forwardAuth-authentik@file`)—that's
  Traefik's file-provider reference convention, applied unconditionally,
  so write the bare middleware name and let `hllc` add it. Like a
  [`router`](#router)'s own `entrypoint`—which joins its own list the
  same way, just without the `@file` suffix—`middleware` needs a router
  to attach to: with no `router` block anywhere there's nothing to
  attach anything to, so naming a middleware anyway is a compile error,
  as `router` describes. The field is service-level, so a service with
  several `router` blocks gets the same middleware list on every one of
  them. The exception is a `router` block that names a list of its own,
  which replaces this one for that router—see
  [Per-router `middleware`](#per-router-middleware).
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
  name to `{ condition: ... }`. `hllc` emits the plain list—unchanged
  from before this syntax existed—as long as *no* entry in the field
  carries a condition, and switches the whole field to the mapping form
  once *any* entry does. A sibling entry with no explicit condition is
  then filled in with `service_started`, since the mapping form
  requires every entry to name one.

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
straight through to the generated YAML. `raw`'s job has narrowed over
time as more of its common uses graduated into dedicated fields—
`privileged`/`devices` most recently—so what's left is the genuine long
tail: real Compose keys that come up rarely enough, or are specific
enough to one deployment, that a dedicated field isn't worth it. `cadvisor`'s
`security_opt` is one:

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
compile error, not a silent override. That's new as of #193—before it,
`raw` was the one field the merge concatenated outright at every tier,
with no uniqueness check at all, so two templates both writing `raw {
user: "..." }` compiled with whichever template's `with` position came
last quietly winning. A key repeated *within one body*—one `raw { }`
block, or two—still isn't checked, matching `raw`'s own schema-free
design: `hllc` extends the accumulated entries rather than rejecting the
repeat, and the generated document keeps only the last value for that
key, the same as any duplicate key inserted into a Rust map twice.

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
`networks` produced—those stay, so a `raw` replacement naming the
same named volume or network still resolves.

## `with`

Not really a "field" you set directly so much as the mechanism for
pulling a `template`'s fields onto a `service`—see [Templates &
Composition](./templates-and-composition.md) for `with` in full.
