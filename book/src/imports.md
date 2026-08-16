# Imports

Real templates and networks are meant to be shared across every service
file in a homelab, not copy-pasted into each one. `use` imports another
`.hll` file under a local alias, so its top-level templates and networks
become available, qualified by that alias.

## Basic usage

```hll
use "docker.hll" as traefik
```

- `use`'s path is always a quoted string, resolved **relative to the
  importing file's own location** — never the entry file's location or
  the directory `hllc` was invoked from.
- `alias.name` then qualifies any reference that would otherwise be a
  bare identifier: a `networks [...]` entry (`networks
  [traefik.traefik-net]`) or a `with` invocation's target (`with
  common.internal_web { ... }`).
- `middleware` and `depends_on` don't support a qualified form. Neither
  has a coherent cross-file meaning: `depends_on` names a same-file
  sibling service, and `middleware` isn't resolved against anything at
  all — it's just a Traefik middleware name passed through verbatim.

## Splitting a homelab across files

The templates from the [previous chapter's example](./templates-and-composition.md#a-complete-example)
split across three files, `use`-connected instead of copy-pasted into
every service:

```hll,file=network.hll,group=imports-example
# network.hll
network traefik-net {
  external
  name: "docker_default"
}
```

```hll,file=templates.hll,group=imports-example
# templates.hll
use "network.hll" as net

template internal_web(port: Number) {
  networks [net.traefik-net]
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
```

```hll,file=syncthing.hll,group=imports-example,entry
# syncthing.hll
use "templates.hll" as common

service syncthing {
  with common.internal_web { port: 8384 }, common.authenticated, common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

Compiling `syncthing.hll` with `hllc --build` produces byte-identical
output to writing all three declarations in one file — `use` is purely
an organizational tool, not a different composition mechanism.

## Two rules that matter for multi-file layouts

**Templates are lexically scoped, not dynamically scoped.** A template's
own references always resolve against *the file it was written in*, not
whichever file happens to call it. In the example above,
`internal_web`'s `networks [net.traefik-net]` resolves against
`templates.hll`'s own `use "network.hll" as net` — even though it's
`syncthing.hll` that actually invokes `internal_web` via `with`.
`syncthing.hll` never itself needs to `use "network.hll"` for this to
work.

**Imports are not transitive.** `use`-ing a file only makes *that file's
own* top-level declarations available under your alias — not anything it
in turn `use`s. In the example above, `syncthing.hll` uses
`templates.hll`, and `templates.hll` uses `network.hll`, but
`syncthing.hll` cannot write `net.traefik-net` itself — only
`templates.hll`'s own template bodies can reach `network.hll`'s
declarations, via the lexical-scoping rule above. If `syncthing.hll`
needed to reference `traefik-net` directly (not through a template), it
would need its own `use "network.hll" as net`.

Together, these two rules mean: a template file needs `use` declarations
for whatever *it* references, and a service file needs `use` declarations
only for what *it* references directly — importing a template doesn't
also import that template's own imports.
