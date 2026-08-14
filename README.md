# hl-lang

[![CI](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml)

`hl-lang` is a small declarative DSL that transpiles to Docker Compose YAML
plus Traefik labels, so that standing up a new homelab service doesn't mean
rewriting a near-identical Compose block + label set every time. It's a
transpiler, not an interpreter — no evaluation, no closures, no runtime —
and doubles as a "learn to write a language" project covering the
lexer → parser → AST → codegen pipeline.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's motivation, grammar,
and worked examples.

## Status

**Lexer milestone only.** The lexer (`crates/hl-lexer`) and a thin CLI stub
that prints its token stream (`crates/hl-cli`) exist; the parser, AST, and
codegen stages described in the design doc are not implemented yet.

## Layout

```
hl-lang/
  crates/
    hl-lexer/   # the lexer: source text -> Token stream
    hl-cli/     # `hl-cli <file.hll>` — lexes a file and prints its tokens
```

## Building & testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Try the CLI against an `.hll` file:

```sh
cargo run -p hl-cli -- crates/hl-lexer/tests/fixtures/jellyfin.hll
```

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and
`crates/hl-lexer/src/lib.rs` for the lexer's rustdoc (token kinds, span
semantics, error types).
