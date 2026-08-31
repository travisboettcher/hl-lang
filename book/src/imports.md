# Imports

Real templates and networks should span every service file in a homelab
instead of getting copy-pasted into each one. `use` imports another
`.hll` file under a local alias, so its top-level templates and networks
become available, qualified by that alias.

## Basic usage

```hll
use "docker.hll" as traefik
```

- `use`'s path is always a quoted string, resolved **relative to the
  importing file's own location**—never the entry file's location or
  the directory you invoked `hllc` from.
- `alias.name` then qualifies any reference that would otherwise be a
  bare identifier: a `networks [...]` entry (`networks
  [traefik.traefik-net]`), a named-volume mount's host side, or a `with`
  invocation's target (`with common.internal_web { ... }`).
- `dns`, `env_file`, `depends_on`, and a router's own `entrypoints` and
  `middleware` lists don't support a qualified form. None has a coherent
  cross-file meaning: `depends_on` names a same-file sibling service,
  `dns` and `env_file` name an IP address and a path on disk, and an
  entry point or a middleware belongs to the deployment's own
  `traefik.yml`—each is just text passed through verbatim. Only `networks` and a named-volume mount's host side resolve
  a qualifier, since only they name something another `.hll` file
  actually declares.

## Splitting a homelab across files

The templates from the [previous page's example](./templates-and-composition.md#a-complete-example)
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

template internal_web(port) {
  networks [net.traefik-net]
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
```

```hll,file=syncthing.hll,group=imports-example,entry
# syncthing.hll
use "templates.hll" as common

volume syncthing-config {}

service syncthing {
  with common.internal_web { port: 8384 }, common.authenticated, common.linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume syncthing-config -> "/config"
}
```

Compiling `syncthing.hll` with `hllc --build` produces byte-identical
output to writing all three declarations in one file—`use` is purely
an organizational tool, not a different composition mechanism.

A named volume is a *reference*, not a string, so it imports the same
way a `network` does. The preceding example declares
`volume syncthing-config {}` in the entry file and mounts it by its bare
name, which resolves against that file's own declarations. Move the
declaration into a shared file and the mount picks up the alias:

```hll,file=storage.hll,group=imported-volume
# storage.hll
volume media {
  external
  name: "media_store"
}
```

```hll,file=jellyfin.hll,group=imported-volume,entry
# jellyfin.hll
use "storage.hll" as storage

service jellyfin {
  image "jellyfin/jellyfin:latest"
  volume storage.media -> "/data"
}
```

The imported declaration's own options—`external`, `name`, `driver`,
`driver_opts`—come with it into the generated `volumes:` section. Only
the *unquoted* form is a reference: a quoted host side, such as
`volume "/mnt/media" -> "/data"`, is a bind-mount path, which names
something on the host rather than anything an `.hll` file declares, so
it takes no alias.

## Two rules that matter for multi-file layouts

**Templates are lexically scoped, not dynamically scoped.** A template's
own references always resolve against *the file that declared it*, not
whichever file happens to call it. In the preceding example,
`internal_web`'s `networks [net.traefik-net]` resolves against
`templates.hll`'s own `use "network.hll" as net`—even though it's
`syncthing.hll` that actually invokes `internal_web` via `with`.
`syncthing.hll` never itself needs to `use "network.hll"` for this to
work.

**Imports aren't transitive.** `use`-ing a file only makes *that file's
own* top-level declarations available under your alias—not anything it
in turn `use`s. In the preceding example, `syncthing.hll` uses
`templates.hll`, and `templates.hll` uses `network.hll`, but
`syncthing.hll` can't write `net.traefik-net` itself—only
`templates.hll`'s own template bodies can reach `network.hll`'s
declarations, via the preceding lexical-scoping rule. If `syncthing.hll`
needed to reference `traefik-net` directly instead of through a template,
it would need its own `use "network.hll" as net`.

Together, these two rules mean: a template file needs `use` declarations
for whatever *it* references, and a service file needs `use` declarations
only for what *it* references directly—importing a template doesn't
also import that template's own imports.

## Two networks, or two volumes, can't share one bare name

An imported `network` keeps its own bare name in the generated
Compose—`net.traefik-net` becomes the `traefik-net` key under
`networks:`. So a file that pulls in an imported network while also
declaring one of its own by the same name is asking for two different
networks under one key, and `hllc` rejects it:

```hll,ignore
use "network.hll" as net

# error: `net.proxy` collides with another network named `proxy`
network proxy {
  name: "local_real_name"
}

service web {
  image "nginx"
  networks [net.proxy]
}
```

Rename one of the two and the ambiguity goes away. The same applies to
two *imported* networks sharing a bare name—`use`-ing both `a.hll` and
`b.hll` is fine, and referencing `a.proxy` and `b.proxy` from the same
file is what's rejected.

Note this only triggers when a qualified reference actually pulls the
imported network in. Two files each declaring their own `network proxy`
is perfectly normal, and stays legal for as long as nothing reaches
across the import to name the other one.

Named volumes follow the same rule, word for word, because an imported
`volume` likewise keeps its own bare name as its key under `volumes:`.
Mounting `storage.media` in a file that also declares its own
`volume media { ... }` is the same ambiguity, and `hllc` reports it the
same way:

```text
jellyfin.hll:6:10: `storage.media` collides with another volume named `media` already in scope — volumes are resolved by their bare name, so the two can't be told apart; rename one of them
```

## `defaults` is the one template `use` can't share

`hllc` looks up the implicit `defaults` template (see
[Templates & Composition](./templates-and-composition.md)) only in the
entry file—the one it was actually pointed at. A
`template defaults { ... }` in an imported file is **ignored**: nothing
errors, the services just don't get those fields. This falls out
of `defaults` having no invocation to resolve—there's no `with
common.defaults` to write, and no alias for the lookup to go through.

The compiler doesn't drop it quietly, though. `hllc` warns, names the
file that declares the stray `defaults`, and carries on—see
[Warnings](./cli.md#warnings):

```text
common.hll:1:10: warning: template `defaults` is declared in an imported file and is not applied — `defaults` is only looked up in the entry file; give it an ordinary name and apply it with `with`
```

So if several service files should share a set of baseline fields, give
the shared template an ordinary name and apply it explicitly:

<!-- vale Google.EmDash = NO -->
```hll,file=common.hll,group=defaults-not-shared
# common.hll — naming this template `defaults` instead would leave it
# unapplied (and warned about) in every file that imports this one
template baseline {
  restart unless-stopped
}
```
<!-- vale Google.EmDash = YES -->

```hll,file=syncthing.hll,group=defaults-not-shared,entry
# syncthing.hll
use "common.hll" as common

service syncthing {
  with common.baseline
  image "lscr.io/linuxserver/syncthing:latest"
}
```

Each file may still declare its own `defaults` for its own services.

## Only the entry file contributes services

`use` shares *declarations*—templates and networks—not services. Only
the file you point `hllc` at contributes `service` blocks to the output.
`hllc` parses a `service` in an imported file, so its syntax and
duplicate names still get checked, and then drops it, since nothing can
reference a service across files in the first place.

That's another warning rather than an error, because the imported file
is usually still doing its real job as a template library:

```text
common.hll:6:9: warning: service `db` is declared in an imported file and is not compiled — only the entry file's services are built
```

If you meant to build that service, point `hllc` at its own file, or, in
a directory build, give it a directory of its own—see
[The `hllc` command-line tool](./cli.md#directory-co-located-mode). If
you meant to share it, what you want is a `template`, applied with
`with`.
