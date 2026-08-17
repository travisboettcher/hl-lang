# hl-lang

[![CI](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml)
[![Docs](https://github.com/travisboettcher/hl-lang/actions/workflows/docs.yml/badge.svg)](https://travisboettcher.github.io/hl-lang/)

`hll` (pronounced "hell" — short for **H**ome**L**ab **L**anguage) is a
small declarative DSL that transpiles to Docker Compose YAML plus Traefik
labels, so that standing up a new homelab service doesn't mean rewriting a
near-identical Compose block + label set every time. It's a transpiler, not
an interpreter — no evaluation, no closures, no runtime — and doubles as a
"learn to write a language" project covering the lexer → parser → AST →
codegen pipeline. Source files use the `.hll` extension; the CLI binary is
`hllc`.

**[Read the user guide](https://travisboettcher.github.io/hl-lang/)** for
syntax, every built-in field, templates/composition, imports, and the
`hllc` CLI, or see [`docs/DESIGN.md`](docs/DESIGN.md) for the language's
formal grammar and worked examples (the implementer-facing spec the user
guide is built on top of).

## Status

**The full pipeline is implemented**: lexer → parser → template/`with`
composition → cross-file `use` imports → codegen → CLI. A `.hll` file can
declare `network`/`service`/`image`/`expose`/`volume`/`env`/`restart`/`raw`,
compose reusable `template`s onto a service via `with`, and `use` another
`.hll` file under a local alias to reuse its templates/networks across
files (`use "docker.hll" as traefik`, then e.g.
`networks [traefik.traefik-net]`) — see docs/DESIGN.md's Composition and
Imports sections. `hllc --build` runs the whole pipeline end to end,
printing the generated Compose YAML to stdout or, with `--out`, writing
it to disk.

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
    hl-cli/      # `hllc <file.hll>` lexes; `--parse` parses and prints
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

MSRV is 1.88 (the workspace uses let-chains, stabilized in edition 2024 as
of that release). `rust-toolchain.toml` pins a specific newer stable for
local builds and most of CI; a dedicated CI job builds against 1.88
itself to keep that floor honest.

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
#
# `--target x86_64-unknown-linux-gnu` is not optional on Linux:
# cargo-fuzz defaults to the musl triple there, which is usually not
# installed and is incompatible with ASan's sanitizer anyway (static
# musl libc isn't sanitizer-compatible). CI passes the same flag — see
# .github/workflows/ci.yml.
cargo install cargo-fuzz --locked
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_lex -- -max_total_time=60
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_parse -- -max_total_time=60
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_pipeline -- -max_total_time=60

# Mutation testing — CI fails on any missed mutant. The timeout
# multiplier matches CI's, and matters: `.cargo/mutants.toml` documents
# mutants that hang rather than fail.
cargo install cargo-mutants --locked
cargo mutants --workspace --timeout-multiplier 3

# Advisories, licenses, and dependency bans — see deny.toml.
cargo install cargo-deny --locked
cargo deny check
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

Nothing here is published to crates.io. A "release" means a git tag plus a
GitHub Release carrying the `hllc` binary — `release-plz.toml`'s
`[workspace] publish = false` + `git_only = true` is what encodes that.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and each
crate's rustdoc (`crates/hl-lexer/src/lib.rs`, `crates/hl-parser/src/lib.rs`,
`crates/hl-linker/src/lib.rs`, `crates/hl-codegen/src/lib.rs`) for
implementation details (token/AST shapes, span semantics, error types).

## What a version number promises

**1.0 means a stable `.hll` language and a stable `hllc` CLI — explicitly
not a stable Rust API.**

The product is the `hllc` compiler and the `.hll` files you feed it. The
five crates under `crates/` are the implementation of that compiler, not a
library offered to third parties, and they are versioned in lockstep with
it purely so there's one number to reason about. Freezing their Rust API
would mean that adding a single new diagnostic variant, or a single new
built-in field to `ServiceFields`, is a breaking change — which is exactly
the kind of change this project expects to keep making.

Covered by the version number — a change that breaks any of these is a
major bump (post-1.0), and must be called out either way:

- **`.hll` source compatibility.** A `.hll` file that compiled before
  still compiles, and still means the same thing.
- **The `hllc` CLI contract.** Flag names and their semantics, positional
  arguments, the shape of what lands on stdout vs. stderr, and exit codes.
- **Generated-Compose semantics.** What the emitted YAML *does* when
  `docker compose up` runs it: the services, images, ports, volumes,
  networks, environment, and Traefik labels it describes.

Not covered — these can change in any release, including a patch:

- **Exact error text.** Wording, phrasing, span rendering, and hints in
  diagnostics are free to improve. Don't grep for them; a machine-readable
  diagnostic format would be a separate, deliberately-specified feature.
- **Exact YAML key ordering and formatting.** Byte-for-byte output
  stability isn't promised — only what the document means to Compose.
  Diffing generated output across `hllc` versions may show churn.
- **The Rust API of the `hl-*` crates.** Type layouts, public fields,
  error enum variants (none are `#[non_exhaustive]`), function
  signatures, module paths — all implementation detail, changeable at
  will. Don't depend on these crates as libraries; depend on the `hllc`
  binary and pin it to a tag.

Because the Rust API is deliberately outside the contract, this repo does
**not** run `cargo-semver-checks`: it would gate the one surface that is
explicitly not promised, and would fail on precisely the changes the
project wants to make freely.

Pre-1.0 (`0.x`), the same three covered surfaces are what `semver-*`
labels describe, but a breaking change is still just a minor bump, per
the usual 0.x convention. Crossing to 1.0.0 is a deliberate act, not an
arithmetic consequence of a `semver-major` label: `release.yml` refuses to
bump `0.x` to `1.0.0` unless the merged PR also carries a `release-1.0`
label. See CONTRIBUTING.md for how to pick a label.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the PR workflow (linked
issue + semver label required) and local checks to run before requesting
review.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
