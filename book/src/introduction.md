# Introduction

`hll` (pronounced "hell"—short for **H**ome**L**ab **L**anguage) is a
small declarative language for describing homelab services. You write a
compact `.hll` file describing a service—its image, the port it
exposes, its volumes, environment variables, restart policy—and `hllc`,
the `hll` compiler, transpiles it into a Docker Compose YAML file with
Traefik reverse-proxy labels already attached.

It exists to remove copy-paste. Standing up a new homelab service with
Docker Compose and Traefik usually means duplicating a near-identical
Compose service block and label set, changing only the image, the port,
and the subdomain. `hll` lets you write just what's different about a
service, and pull in the repeated parts (the Traefik network, the
forward-auth middleware, the `PUID`/`PGID` pair every LinuxServer.io image
wants) from a shared **template**.

`hll` is a transpiler, not an interpreter—there's no evaluation, no
runtime, no state. Every `.hll` file compiles down to plain Compose YAML
that you check in, deploy, and read like any other Compose file.

## Who this book is for

This is a **user guide**: it's for someone writing `.hll` files to
describe their own homelab, not someone modifying the compiler itself. It
covers the language's syntax in plain terms, every built-in field, how
templates and imports work, and the `hllc` command line.

If you're looking for the formal grammar, desugaring rules, or the
internals of the lexer/parser/codegen pipeline, see
[`docs/DESIGN.md`](https://github.com/travisboettcher/hl-lang/blob/main/docs/DESIGN.md)
in the repository instead—the compiler builds against that
implementer-facing spec. This book is a friendlier presentation layer on
top of it, covering the same rules with prose and examples instead of
Backus-Naur Form (BNF).

## A quick taste

```hll,build
service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.example.com"
  volume "/mnt/media" -> "/data"
  env PUID = "1000"
  restart unless-stopped
}
```

`hllc build` turns that into a ready-to-run `docker-compose.yml` with a
`jellyfin` service, its image, a bind mount, an environment variable, a
restart policy, and Traefik labels routing `media.example.com` to port
8096—all inferred from those five lines. The rest of this book walks
through how that works, starting with [Getting Started](./getting-started.md).
