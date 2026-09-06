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

Compiling `syncthing.hll` with `hllc build` produces byte-identical
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

## Reading an imported declaration's name

Add a third segment and the same alias reads a field off what it names
rather than referencing it—`net.traefik-net.name` is the real Docker
name of that imported network, `docker_default` in the preceding
example. See [Reading a declaration's real
name](./templates-and-composition.md#reading-a-declarations-real-name)
for the field itself. Two things about it are specific to imports:

```hll,file=proxy.hll,group=imported-name
# proxy.hll
network proxy {
  external
  name: "docker_default"
}
```

```hll,file=caddy.hll,group=imported-name,entry
# caddy.hll
use "proxy.hll" as net

service jellyfin {
  image "jellyfin/jellyfin"
  labels {
    "caddy.network": net.proxy.name
  }
}
```

**Reading a name imports nothing.** The generated document here carries
no `networks:` section at all: `networks [net.proxy]` is what pulls a
declaration across an import, and reading its name yields a plain
string. That also keeps it clear of the bare-name collision the next
section describes, since no second declaration comes over to collide.

**The alias resolves in the file holding the access.** Inside a template,
`net.proxy.name` reads the `net` of the file that declared the
template—the same lexical-scoping rule as any other reference, and it
covers a `with`-invocation's arguments too, since those are values the
calling file wrote.

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

## Sharing a set of baseline fields

Every template is shareable, because naming a template in a `with` is
the one way any template reaches a service. So a baseline several service files
should agree on lives in one imported file, and each service applies it:

```hll,file=common.hll,group=shared-baseline
# common.hll
template baseline {
  restart unless-stopped
}
```

```hll,file=syncthing.hll,group=shared-baseline,entry
# syncthing.hll
use "common.hll" as common

service syncthing {
  with common.baseline
  image "lscr.io/linuxserver/syncthing:latest"
}
```

`hllc` used to apply a template named exactly `defaults` on its own, and
that one template was the one `use` could never share: with no
invocation, there was no alias for a cross-file lookup to go through, so
an imported `defaults` reached nothing and warned. That special case no
longer exists. `defaults` is an ordinary name now, and
`with common.defaults` reaches an imported one exactly as
`with common.baseline` does—see
[Templates & Composition](./templates-and-composition.md).

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

## Modules bundled with the compiler

`hllc` can carry `.hll` modules inside its own binary. A `use` path that
starts with `std:` names one of those instead of a file beside yours:

```hll
use "std:traefik" as traefik
```

That path resolves against the compiler's own modules rather than
against your tree. This compiler bundles none yet, so every `std:` path
you can write today ends in one diagnostic:

```text
service.hll:1:5: unknown standard library module "std:traefik" — this compiler bundles no standard library modules
```

The first module to ship there is a Traefik template that reproduces the
labels `hllc` generates for a `router` block, and it arrives with that
migration rather than ahead of it. Until it does, the prefix earns its
place in this page for one reason: `std:` belongs to the compiler. A
file of your own named `std:something.hll` no longer answers to
`use "std:something.hll"`, and reaching it takes an explicit relative
spelling, `use "./std:something.hll"`.

Once a module does ship, it behaves like any other import. An alias
qualifies its declarations the same way, its templates resolve against
the module that declared them, and its own imports stay its own. The one
thing it never does: come from anywhere but the binary. No search path,
no environment variable, no directory in your home, nothing fetched over
a network. A bundled module and the compiler that reads it ship as one
artifact and move together, which also means a compiler upgrade can
change what one of them generates.
