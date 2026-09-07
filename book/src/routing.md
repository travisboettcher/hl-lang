# Routing

Nothing on this page is part of the language.

`hll` has no `router` field, no matcher grammar, no idea what a
hostname is for. Routing is Docker labels, labels are a
[built-in field](./built-in-fields.md#labels), and the labels a
reverse proxy wants come from templates like any others. This page
covers one set of templates the compiler happens to ship.

That's the point rather than an omission. The test every built-in has
to pass is *would this make sense on a homelab with completely
different infrastructure?*—and a `router` block only means something if
you run Traefik. Earlier versions of `hllc` did answer that question in
the grammar, and walking it back is what makes `hll` a language about
Compose services rather than a language about one person's reverse
proxy. [What the compiler stopped
knowing](#what-the-compiler-stopped-knowing) covers the loss. The gain:
Caddy, nginx, or a proxy that doesn't exist yet is a file you write, not
a compiler you fork.

## Getting the templates

`std:traefik` ships inside `hllc`. There is no file to vendor and no
path to get right—it resolves through the same `use` an ordinary import
does, so [Imports](./imports.md) applies to it unchanged:

```hll,build
use "std:traefik" as traefik

service web {
  image "nginx"
  with traefik.http { host: "web.example.com", port: 8080 }
}
```

```text
services:
  web:
    image: nginx
    expose:
    - 8080
    labels:
    - traefik.http.routers.web.rule=Host(`web.example.com`)
    - traefik.http.services.web.loadbalancer.server.port=8080
```

The alias is yours to pick. `traefik` reads well and this page uses
it, but nothing depends on the name.

## The common case

`http` is the shape most services want: one router, one hostname, one
port. It writes the `expose` too, which is what keeps the port written
once instead of twice.

`http_named` is the same shape for one of several routers on a service,
building the `<service>-<name>` router id from the name you pass.

Both composites write the service's `expose`, though, which means you
can apply *one* of them per service: two would each set `expose.port`
and collide. A service with two routers drops to the primitives below,
where the service writes its own `expose` once:

```hll,build
use "std:traefik" as traefik

service web {
  image "nginx"
  expose 8080
  with
    traefik.http_rule { router: "{{name}}-public", rule: "Host(`web.example.com`)" },
    traefik.http_rule { router: "{{name}}-admin", rule: "Host(`admin.example.com`)" },
    traefik.port { port: 8080 }
}
```

```text
services:
  web:
    image: nginx
    expose:
    - 8080
    labels:
    - traefik.http.routers.web-public.rule=Host(`web.example.com`)
    - traefik.http.routers.web-admin.rule=Host(`admin.example.com`)
    - traefik.http.services.web.loadbalancer.server.port=8080
```

Naming one template twice in a `with` list is how you say "two of
these." No template needs a plural form.

## One template per label

Under the composites is a flat set of templates, one for each label a
router can carry:

| Template | Writes |
|---|---|
| `http_rule(router, rule)` | the router's `rule` |
| `http_entrypoints(router, entrypoints)` | its `entrypoints` |
| `http_middlewares(router, middlewares)` | its `middlewares` |
| `http_priority(router, priority)` | its `priority` |
| `http_service(router, port)` | a Traefik service of its own, and the pointer to it |
| `tcp_*` | the same five, one segment over, for TCP routers |
| `port(port)` | the load-balancer target every router falls back to |
| `docker_network(net)` | `traefik.docker.network`, from a network declaration |
| `disable()` | `traefik.enable=false`, and nothing else |

One per label rather than one `router` template with optional fields,
because a `labels` block emits every key it lists and the language has
no way to omit one. A single template taking every option would write
an empty `entrypoints=` for a router that has none. So each label a
router may or may not carry is its own template, and a caller lists the
ones it wants:

```hll,build
use "std:traefik" as traefik

service web {
  image "nginx"
  expose 8080
  with
    traefik.http_rule { router: "{{name}}", rule: "Host(`web.example.com`)" },
    traefik.http_entrypoints { router: "{{name}}", entrypoints: ["web-secure", "web"] },
    traefik.http_priority { router: "{{name}}", priority: 42 },
    traefik.port { port: 8080 }
}
```

```text
services:
  web:
    image: nginx
    expose:
    - 8080
    labels:
    - traefik.http.routers.web.rule=Host(`web.example.com`)
    - traefik.http.routers.web.entrypoints=web-secure,web
    - traefik.http.routers.web.priority=42
    - traefik.http.services.web.loadbalancer.server.port=8080
```

`router` is the full router id, not a name the template decorates. Pass
`"{{name}}"` for the one unnamed router a service has, `"{{name}}-api"`
for a named one—[interpolation](./templates-and-composition.md) resolves
inside an argument, so you never type the service's own name out.

A list argument joins with commas, which is what `entrypoints` and
`middlewares` want. See [Passing a
list](./templates-and-composition.md#passing-a-list).

## Rules are strings

`rule` takes Traefik's rule syntax as text, backticks and all:

```hll,fragment
with traefik.http_rule {
  router: "{{name}}"
  rule: "Host(`web.example.com`) && !PathPrefix(`/admin`)"
}
```

`hllc` doesn't parse it. It checks that the value is a well-formed
string, substitutes any `{{...}}` in it, and writes it out—which is the
whole of what it does to any label value. See [A value goes through as
written](./built-in-fields.md#a-value-goes-through-as-written).

## Composing middlewares

A middlewares entry takes a **list**, and the shape is load-bearing.
A list-valued `labels` entry concatenates across template tiers instead
of colliding, so a template that adds one middleware to whatever it's
mixed into is expressible as its own unit:

```hll,build
use "std:traefik" as traefik

template internal {
  labels { "traefik.http.routers.{{name}}.middlewares": ["local-ipwhitelist@file"] }
}

template authenticated {
  labels { "traefik.http.routers.{{name}}.middlewares": ["forwardAuth-authentik@file"] }
}

service syncthing {
  image "lscr.io/linuxserver/syncthing:latest"
  with traefik.http { host: "syncthing.example.com", port: 8384 }, internal, authenticated
}
```

```text
services:
  syncthing:
    image: lscr.io/linuxserver/syncthing:latest
    expose:
    - 8384
    labels:
    - traefik.http.routers.syncthing.rule=Host(`syncthing.example.com`)
    - traefik.http.services.syncthing.loadbalancer.server.port=8384
    - traefik.http.routers.syncthing.middlewares=local-ipwhitelist@file,forwardAuth-authentik@file
```

Written as single values those two templates would be two answers to a
one-answer question, and `hllc` would refuse the pair. See [A list value
composes instead of
colliding](./built-in-fields.md#a-list-value-composes-instead-of-colliding)
for the rule itself, which is about lists rather than about routing.

## The Docker network label

Traefik needs to know which network to reach a multi-homed container
on. `docker_network` writes it, and reads the network's *real* Docker
name—the `name:` override when the declaration sets one, the identifier
otherwise—rather than making you repeat it:

```hll,build
use "std:traefik" as traefik

network proxy {
  external
  name: "docker_default"
}

service web {
  image "nginx"
  networks [proxy]
  with traefik.http { host: "web.example.com", port: 8080 },
       traefik.docker_network { net: proxy }
}
```

```text
services:
  web:
    image: nginx
    networks:
    - proxy
    expose:
    - 8080
    labels:
    - traefik.http.routers.web.rule=Host(`web.example.com`)
    - traefik.http.services.web.loadbalancer.server.port=8080
    - traefik.docker.network=docker_default
networks:
  proxy:
    name: docker_default
    external: true
```

`{{net.name}}` inside the template is a [field
access](./templates-and-composition.md), which is a general facility: a
declaration holds values and the language can read them. Nothing here
is special-cased for Traefik.

## Keeping Traefik off a service

`disable` writes `traefik.enable=false` and nothing else:

```hll,build
use "std:traefik" as traefik

service db {
  image "postgres:15"
  with traefik.disable
}
```

```text
services:
  db:
    image: postgres:15
    labels:
    - traefik.enable=false
```

A zero-parameter template needs no argument body, so `with
traefik.disable` is the whole invocation.

## What the compiler stopped knowing

Worth saying plainly, because this is a real trade rather than a free win.

When routing was a built-in, `hllc` understood what a rule meant. It
parsed the matcher expression, checked matcher names, checked argument
counts, and refused a `path_prefix` beside a `rule` that already said
where to route. A typo in `PathPrefx` was a compile error.

None of that survives. A label value is a string, and a misspelled
matcher inside one is a string with a typo in it—`hllc` writes it out
and Traefik declines to match anything. The checks you keep are
the ones that belong to the language rather than to Traefik: a
duplicate label key, two entries that resolve to one key, an unknown
interpolation, an unsubstituted parameter, a template invoked with the
wrong arguments.

Adding a Traefik rule validator back would mean the compiler tracking a
third party's syntax across its releases, which is the coupling this
page exists to undo. The check moved to where someone maintains it:
Traefik's own startup, which reports a rule it can't parse.

## Writing your own

There is nothing privileged about `std:traefik`. It's a `.hll` file of
ordinary templates that happens to travel inside the compiler, and a
Caddy or nginx equivalent is the same file in your own repo:

```hll,build
network proxy {
  external
  name: "docker_default"
}

template caddy(net, host, port) {
  expose $port
  networks [$net]
  labels {
    "caddy": "{{host}}"
    "caddy.reverse_proxy": "{{name}}:{{port}}"
    "caddy.network": "{{net.name}}"
  }
}

service jellyfin {
  image "jellyfin/jellyfin"
  with caddy { net: proxy, host: "media.example.com", port: 8096 }
}
```

```text
services:
  jellyfin:
    image: jellyfin/jellyfin
    networks:
    - proxy
    expose:
    - 8096
    labels:
    - caddy=media.example.com
    - caddy.reverse_proxy=jellyfin:8096
    - caddy.network=docker_default
networks:
  proxy:
    name: docker_default
    external: true
```

Same fields, same composition rules, same interpolation. The only thing
`std:traefik` has that this doesn't is a shorter `use` line.
