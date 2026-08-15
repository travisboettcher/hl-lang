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

## Installing

Every merge to main cuts a new [tagged release](https://github.com/travisboettcher/hl-lang/releases)
with a prebuilt `hllc` Linux x86-64 binary attached — download it,
`chmod +x`, and put it on your `PATH`, no Rust toolchain required:

```sh
curl -Lo hllc https://github.com/travisboettcher/hl-lang/releases/latest/download/hllc-linux-x86_64
chmod +x hllc
./hllc --build <file.hll>
```

Consumers (CI, a local deploy step) should pin to a specific tag rather than
`latest` for reproducibility. See "Building & testing" below to build from
source instead — e.g. for a platform with no release binary.

## Building & testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

CI also gates on coverage and runs fuzz and mutation testing:

```sh
# Coverage gate: CI fails if workspace-wide line coverage drops below 80%.
cargo install cargo-llvm-cov --locked
cargo llvm-cov --workspace --html --fail-under-lines 80

# Fuzz testing (lexer, parser, and the full parse -> compose -> codegen
# pipeline) — requires nightly. CI runs a 60s smoke test per target on
# every PR, plus a longer nightly scheduled run.
cargo install cargo-fuzz --locked
cargo +nightly fuzz run fuzz_lex -- -max_total_time=60
cargo +nightly fuzz run fuzz_parse -- -max_total_time=60
cargo +nightly fuzz run fuzz_pipeline -- -max_total_time=60

# Mutation testing — informational, non-blocking in CI.
cargo install cargo-mutants --locked
cargo mutants --workspace
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

## Releasing

Releases are fully automated (`.github/workflows/release.yml`) — there's no
manual tag to push. Every PR into main must carry exactly one of the
`semver-major`/`semver-minor`/`semver-patch` labels (enforced by
`semver-label.yml` as a required check); merging bumps every crate's version
accordingly, opens and auto-merges a small `chore(release)` PR with that
change, and that merge cuts the actual tagged release with a rebuilt `hllc`
binary attached. See `release-plz.toml` and the workflow's own comments for
how the pieces fit together.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and each
crate's rustdoc (`crates/hl-lexer/src/lib.rs`, `crates/hl-parser/src/lib.rs`,
`crates/hl-linker/src/lib.rs`, `crates/hl-codegen/src/lib.rs`) for
implementation details (token/AST shapes, span semantics, error types).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the PR workflow (linked
issue + semver label required) and local checks to run before requesting
review.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
