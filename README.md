# hl-lang

[![CI](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml)

`hll` (pronounced "hell" — short for **H**ome**L**ab **L**anguage) is a
small declarative DSL that transpiles to Docker Compose YAML plus Traefik
labels, so that standing up a new homelab service doesn't mean rewriting a
near-identical Compose block + label set every time. It's a transpiler, not
an interpreter — no evaluation, no closures, no runtime — and doubles as a
"learn to write a language" project covering the lexer → parser → AST →
codegen pipeline. Source files use the `.hll` extension; the CLI binary is
`hllc`.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's motivation, grammar,
and worked examples.

## Status

**The full pipeline is implemented**: lexer → parser → template/`with`
composition → cross-file `use` imports → codegen → CLI. A `.hll` file can
declare `network`/`service`/`image`/`expose`/`volume`/`env`/`restart`/`raw`,
compose reusable `template`s onto a service via `with`, and `use` another
`.hll` file under a local alias to reuse its templates/networks across
files (`use "docker.hll" as traefik`, then e.g.
`networks [traefik.traefik-net]`) — see docs/DESIGN.md's Composition and
Imports sections. `hl-cli --build` runs the whole pipeline end to end and
writes real Compose YAML.

## Layout

```
hl-lang/
  crates/
    hl-lexer/    # the lexer: source text -> Token stream
    hl-parser/   # the parser (Token stream -> AST) and compose (AST ->
                 # fully-merged ComposedProgram, resolving template/`with`
                 # and, given a SymbolResolver, cross-file `use` imports)
    hl-linker/   # loads a real `use` graph off disk (or, for tests, an
                 # in-memory map) and implements hl-parser's SymbolResolver
                 # over it
    hl-codegen/  # ComposedProgram -> Docker Compose YAML + Traefik labels
    hl-cli/      # `hl-cli <file.hll>` lexes; `--parse` parses and prints
                 # the AST; `--build [--out <path>]` runs the full
                 # pipeline (link -> compose -> codegen)
```

## Building & testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Try the CLI (`hllc`) against an `.hll` file — `cargo run -p hl-cli --` runs
it straight from source without a separate install step:

```sh
cargo run -p hl-cli -- crates/hl-lexer/tests/fixtures/jellyfin.hll
cargo run -p hl-cli -- --parse crates/hl-parser/tests/fixtures/jellyfin.hll
cargo run -p hl-cli -- --build crates/hl-parser/tests/fixtures/syncthing.hll
```

(`cargo build -p hl-cli` / `cargo install --path crates/hl-cli` produce the
`hllc` binary directly, usable the same way: `hllc --build <file.hll>`.)

`--build` also resolves real cross-file `use` imports — try it against
the split-file example in `crates/hl-cli/tests/fixtures/imports/`
(`network.hll` + `templates.hll` + `syncthing.hll`, connected by `use`
decls), which produces byte-identical output to the single-file version
above:

```sh
cargo run -p hl-cli -- --build crates/hl-cli/tests/fixtures/imports/syncthing.hll
```

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and each
crate's rustdoc (`crates/hl-lexer/src/lib.rs`, `crates/hl-parser/src/lib.rs`,
`crates/hl-linker/src/lib.rs`, `crates/hl-codegen/src/lib.rs`) for
implementation details (token/AST shapes, span semantics, error types).
