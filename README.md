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

**Lexer + parser (built-in types only).** The lexer (`crates/hl-lexer`) and
parser (`crates/hl-parser`) are implemented, plus a CLI (`crates/hl-cli`)
that can lex or parse a file. The parser covers `network`, `service`,
`image`, `expose`, `volume`, `env`, `restart`, and `raw` — `template`/`with`
composition is a fast-follow milestone, not implemented yet. AST → codegen
is also not implemented yet.

## Layout

```
hl-lang/
  crates/
    hl-lexer/   # the lexer: source text -> Token stream
    hl-parser/  # the parser: Token stream -> AST (built-in types only)
    hl-cli/     # `hl-cli <file.hll>` lexes; `hl-cli --parse <file.hll>` parses
```

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

Try the CLI against an `.hll` file:

```sh
cargo run -p hl-cli -- crates/hl-lexer/tests/fixtures/jellyfin.hll
cargo run -p hl-cli -- --parse crates/hl-parser/tests/fixtures/jellyfin.hll
```

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and each
crate's rustdoc (`crates/hl-lexer/src/lib.rs`, `crates/hl-parser/src/lib.rs`)
for implementation details (token/AST shapes, span semantics, error types).
