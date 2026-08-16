# The `hllc` CLI

`hllc` is the `hll` compiler's command-line binary. It has three modes,
selected by flag, all taking a single positional path argument.

```
hllc [--parse | --build] <file.hll or directory> [--out <path>]
```

## Lexing (default, no flags)

With no flags, `hllc` just lexes the file and prints its token stream —
useful for debugging the lexer itself, not something you'd reach for day
to day:

```sh
hllc jellyfin.hll
```

## `--parse`

Parses the file and pretty-prints its AST, without resolving `with`
composition or generating any output. Useful for checking that a file is
syntactically valid, or for understanding how a particular shorthand
desugars, without going all the way to Compose YAML:

```sh
hllc --parse jellyfin.hll
```

## `--build`

Runs the full pipeline — parse, resolve `use` imports, resolve
template/`with` composition, generate Compose YAML — and either prints
the result or writes it to disk. This is the one you'll use in practice.

### Single file

```sh
hllc --build jellyfin.hll                       # prints YAML to stdout
hllc --build jellyfin.hll --out docker-compose.yml  # writes to a path
```

One input file always produces one output document (it may hold multiple
`service` declarations — see [Getting Started](./getting-started.md#adding-a-second-service)).
`--build` fully resolves any `use` graph the file participates in, so
building `syncthing.hll` from the [Imports](./imports.md) example
produces the same output whether its templates live in the same file or
across three `use`-connected ones.

### Directory: flat mode

Point `--build` at a directory instead of a file, and every `.hll` file
**directly inside it** is treated as its own independent entry point,
each with its own `use` graph:

```
services/
  jellyfin.hll
  syncthing.hll
  uptime-kuma.hll
```

```sh
hllc --build services/ --out dist/
```

`--out` is **required** in this mode — with potentially many files'
worth of output, there's no single meaningful default location. Each
file's stem becomes its own output directory:
`dist/jellyfin/docker-compose.yml`, `dist/syncthing/docker-compose.yml`,
and so on.

### Directory: co-located mode

This mode is chosen automatically when the target directory holds **no**
`.hll` files directly, but at least one immediate subdirectory that does
— the layout a real homelab tends to use in practice, keeping each
service's `.hll` source next to its other files (`.env`, bind-mounted
config):

```
services/
  jellyfin/
    jellyfin.hll
    .env
  syncthing/
    syncthing.hll
```

```sh
hllc --build services/
```

With no `--out`, each subdirectory's `.hll` file builds in place, right
back into that same subdirectory: `services/jellyfin/docker-compose.yml`,
`services/syncthing/docker-compose.yml`. An explicit `--out <dir>` still
remaps the whole tree, the same way flat mode's does, keyed by
subdirectory name instead of file stem: `<out>/jellyfin/docker-compose.yml`.

A subdirectory containing more than one `.hll` file is a hard error in
this mode — it's ambiguous which one's output belongs directly in that
subdirectory, so `hllc` won't guess.

### Which directory mode applies

`hllc` inspects the target directory once and picks a mode:

- **Any `.hll` files directly inside it** → flat mode, `--out` required.
- **No `.hll` files directly inside it, but at least one subdirectory
  that has one** → co-located mode.
- **Neither** (no `.hll` files anywhere within one level) → builds
  nothing, successfully.

## Exit codes

`hllc` exits non-zero on any lex/parse/link/compose/codegen error,
printing a `path:line:col: message` diagnostic to stderr — safe to use
directly as a CI gate before `docker compose up`.
