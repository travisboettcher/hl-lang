# `hl-lang`

[![CI](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/travisboettcher/hl-lang/actions/workflows/ci.yml)
[![Docs](https://github.com/travisboettcher/hl-lang/actions/workflows/docs.yml/badge.svg)](https://travisboettcher.github.io/hl-lang/)

`hll` (pronounced "hell"—short for **H**ome**L**ab **L**anguage) is a
small declarative Domain-Specific Language (DSL) that transpiles to Docker
Compose YAML plus Traefik labels, so that standing up a new homelab service
doesn't mean rewriting a near-identical Compose block + label set every
time. It's a transpiler, not an interpreter—no evaluation, no closures, no
runtime—and doubles as a "learn to write a language" project covering the
lexer → parser → Abstract Syntax Tree (AST) → codegen pipeline. Source
files use the `.hll` extension, and the command-line tool binary is
`hllc`.

**[Read the user guide](https://travisboettcher.github.io/hl-lang/)** for
syntax, every built-in field, templates/composition, imports, and the
`hllc` command-line tool, or see [`docs/DESIGN.md`](docs/DESIGN.md) for the
language's formal grammar and worked examples—the implementer-facing spec
underlying the user guide.

## Status

**The compiler implements the full pipeline**: lexer → parser →
template/`with` composition → cross-file `use` imports → codegen →
command-line tool. A `.hll` file declares `network`, `volume` and
`service` blocks. A service carries the image or build context it runs,
the ports it exposes or publishes, its volumes, environment, restart and
health-check policy, one or more Traefik `router` blocks, extra Docker
`labels`, and a `raw` escape hatch for any Compose key the language
doesn't model yet. The user guide lists every field and what it
generates. Services combine reusable `template`s via `with`, and
`use` another `.hll` file under a local alias to reuse its
templates/networks across files (`use "docker.hll" as traefik`, then for
example `networks [traefik.traefik-net]`)—see docs/DESIGN.md's
Composition and Imports sections. `hllc build` runs the whole pipeline
end to end, printing the generated Compose YAML to stdout or, with
`--out`, writing it to disk. `hllc check` runs that same pipeline and
writes nothing, which is what makes it a CI gate.

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
    hl-cli/      # `hllc build <file.hll> [--out <path>]` runs the full
                 # pipeline (link -> compose -> codegen); `hllc check`
                 # runs it and writes nothing; `hllc parse`/`hllc tokens`
                 # print the AST and the token stream
```

## Installing

Every merge to main cuts a new [tagged release](https://github.com/travisboettcher/hl-lang/releases)
with a prebuilt `hllc` Linux x86-64 binary attached—download it,
`chmod +x`, and put it on your `PATH`, no Rust toolchain required:

```sh
curl -Lo hllc https://github.com/travisboettcher/hl-lang/releases/latest/download/hllc-linux-x86_64
chmod +x hllc
./hllc build <file.hll>
```

Consumers—CI, a local deploy step—should pin to a specific tag rather than
`latest` for reproducibility.

**Linux x86-64 is the only platform this project tests or supports.** Every
CI job runs on `ubuntu-latest`, and the preceding binary is the only one a
release ships. The workspace is plain Rust, so `cargo build --workspace`—see
"Building & testing" below—may well work on macOS or Windows too, but
that's untested and unsupported today. Expect rough edges, particularly
around filesystem path handling.

## Building & testing

The Minimum Supported Rust Version (MSRV) is 1.88—the workspace uses
let-chains, stabilized in edition 2024 as of that release.
`rust-toolchain.toml` pins a specific newer stable for local builds and
most of CI, and a dedicated CI job builds against 1.88 itself to keep that
floor honest.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
```

`crates/hl-codegen/tests/golden_tests.rs` keeps its expected Compose YAML
as [`insta`](https://insta.rs) inline snapshots, so a deliberate codegen
change updates those expectations through a review step rather than
through dozens of hand-edited YAML literals:

```sh
cargo install cargo-insta --locked

# Step through each changed snapshot, accepting or rejecting one at a time.
cargo insta review

# Take every pending snapshot at once, once you've read the diffs.
cargo insta accept
```

Read every diff before you accept it. A snapshot states what the compiler
must emit, so taking a new one asserts that the new output is correct—make
that call deliberately rather than to clear a red test. Both `cargo test
--workspace` and CI fail on a mismatch and never rewrite a snapshot.

`crates/hl-cli/tests/cmd/` covers the `hllc` command line itself with
[`trycmd`](https://docs.rs/trycmd) transcripts. Each `.trycmd` file states
an invocation, the stdout and stderr it produces, and its exit code, as
plain text you can read. A `<name>.in/` directory beside it supplies the
working directory those commands run in, and a `<name>.out/` directory
states what that working directory must hold afterwards, which is how the
multi-file `build --out <dir>` cases assert everything they wrote at
once. Bless a deliberate change to the output the same way:

```sh
# Rewrite every transcript step whose output no longer matches.
TRYCMD=overwrite cargo test -p hl-cli --test cli_tests

# Or write each command's real output to crates/hl-cli/dump/ and
# compare it yourself, leaving the transcripts alone.
TRYCMD=dump cargo test -p hl-cli --test cli_tests
```

Read the resulting diff line by line before you commit it. A transcript
states what `hllc` must print and what exit code it must return, so
rewriting one asserts that the new diagnostic wording, the new exit code,
or the newly generated file is what users should now get. CI pins `TRYCMD`
to a value that never writes, so a mismatch there fails the build.

`crates/hl-cli/tests/cases/` is a file-driven regression corpus: one
`.hll` input per case, with what the compiler makes of it recorded beside
it in a `.expected` file—the generated Compose document, or the rendered
diagnostic, or both a warning and a document, each under its own label.
Adding a case means dropping in a file rather than writing a Rust test:

```sh
# Record what the compiler makes of every case that has no expectation
# yet, or whose expectation no longer matches.
SNAPSHOTS=overwrite cargo test -p hl-cli --test cases
```

A case file has to open with a `#` comment saying what it pins down, and
its name becomes the test name—so a case called
`issue_168_healthcheck_param.hll` runs under `cargo test issue_168`, and
a failure names the file. `SNAPSHOTS` is the variable the transcripts
already use, so this adds no further way to update an expected output.
See "Adding a regression case" in `CONTRIBUTING.md` for which cases
belong here and which belong in the hand-written suites.

`crates/hl-cli/tests/compose_differential.rs` hands every document
codegen produces to Docker Compose's own parser. Snapshots and
transcripts prove the output stays stable against expectations this
repo wrote itself, which says nothing about whether Compose agrees—so
this test asks Compose, over the fixtures under
`crates/hl-parser/tests/fixtures/` and every `build`-tagged worked
example in `book/src/`. It also compares Compose's normalized reading
of each document against the generated one, catching a label or an
`expose` entry that Compose accepts but reads differently than codegen
meant it.

An off-by-default Cargo feature gates that test, because no contributor
should need Docker to run `cargo test --workspace`. Enabling the feature
**is** the request to run it, so a missing Compose plugin fails the test
rather than skipping it—a skip would read as coverage in a CI log, which
is worse than no test at all. Nothing needs a running Docker daemon:
`docker compose config` parses and validates on the client side. CI runs
the test in its own job, kept clear of the coverage gate so a Compose
upgrade can't block unrelated pull requests.

CI also gates on coverage and runs fuzz, mutation, and differential
testing:

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
# musl libc isn't sanitizer-compatible). CI passes the same flag—see
# .github/workflows/ci.yml.
cargo install cargo-fuzz --locked
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_lex -- -max_total_time=60
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_parse -- -max_total_time=60
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuzz_pipeline -- -max_total_time=60

# Mutation testing—CI fails on any missed mutant. The timeout
# multiplier matches CI's, and matters: `.cargo/mutants.toml` documents
# mutants that hang rather than fail.
cargo install cargo-mutants --locked
cargo mutants --workspace --timeout-multiplier 3

# Differential test: Docker Compose's own parser validates every
# document codegen produces, over the parser fixtures and the book's
# `build`-tagged worked examples. Needs the Compose v2 plugin, not a
# running daemon. Off by default, and it fails rather than skips when
# the plugin is missing.
cargo test -p hl-cli --features docker-differential --test compose_differential

# Advisories, licenses, and dependency bans — see deny.toml.
cargo install cargo-deny --locked
cargo deny check
```

Try the command-line tool (`hllc`) against an `.hll` file—`cargo run -p
hl-cli --` runs it straight from source without a separate install step:

```sh
cargo run -p hl-cli -- tokens crates/hl-lexer/tests/fixtures/jellyfin.hll
cargo run -p hl-cli -- parse crates/hl-parser/tests/fixtures/jellyfin.hll
cargo run -p hl-cli -- build crates/hl-parser/tests/fixtures/syncthing.hll
cargo run -p hl-cli -- check crates/hl-parser/tests/fixtures/syncthing.hll
```

(`cargo build -p hl-cli` / `cargo install --path crates/hl-cli` produce the
`hllc` binary directly, usable the same way: `hllc build <file.hll>`.)

`build` also resolves real cross-file `use` imports—try it against
the split-file example in `crates/hl-cli/tests/fixtures/imports/`
(`network.hll` + `templates.hll` + `syncthing.hll`, connected by `use`
decls), which produces byte-identical output to the preceding
single-file version:

```sh
cargo run -p hl-cli -- build crates/hl-cli/tests/fixtures/imports/syncthing.hll
```

## Releasing

Releases are fully automated (`.github/workflows/release.yml`)—there's no
manual tag to push. Every PR into main must carry exactly one of the
`semver-major`/`semver-minor`/`semver-patch` labels (enforced by
`semver-label.yml` as a required check). Merging bumps every crate's version
accordingly, opens and auto-merges a small `chore(release)` PR with that
change, and that merge cuts the actual tagged release with a rebuilt `hllc`
binary attached. See `release-plz.toml` and the workflow's own comments for
how the pieces fit together.

This project doesn't publish anything to crates.io. A "release" means a
git tag plus a GitHub Release carrying the `hllc` binary—`release-plz.toml`'s
`[workspace] publish = false` + `git_only = true` is what encodes that.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the language's grammar, and each
crate's rustdoc (`crates/hl-lexer/src/lib.rs`, `crates/hl-parser/src/lib.rs`,
`crates/hl-linker/src/lib.rs`, `crates/hl-codegen/src/lib.rs`) for
implementation details—token/AST shapes, span semantics, error types.

## What a version number promises

**1.0 means a stable `.hll` language and a stable `hllc` command-line
tool—explicitly not a stable Rust API.**

The product is the `hllc` compiler and the `.hll` files you feed it. The
five crates under `crates/` are the implementation of that compiler, not a
library offered to third parties, and the project versions them in
lockstep with it purely so there's one number to reason about. Freezing
their Rust API would mean that adding a single new diagnostic variant, or
a single new built-in field to `ServiceFields`, is a breaking change—which
is exactly the kind of change this project expects to keep making.

Covered by the version number—a change that breaks any of these is a
major bump once the project passes 1.0, and the project calls it out
either way:

- **`.hll` source compatibility.** A `.hll` file that compiled before
  still compiles, and still means the same thing.
- **The `hllc` command-line tool contract.** Subcommand and flag names
  and their semantics, positional arguments, the shape of what lands on
  stdout vs. stderr, and exit codes.
- **Generated-Compose semantics.** What the emitted YAML *does* when
  `docker compose up` runs it: the services, images, ports, volumes,
  networks, environment, and Traefik labels it describes.

Not covered—these can change in any release, including a patch:

- **Exact error text.** Wording, phrasing, span rendering, and hints in
  diagnostics are free to improve. Don't grep for them. A machine-readable
  diagnostic format would be a separate, deliberately specified feature.
- **Exact YAML key ordering and formatting.** Byte-for-byte output
  stability isn't promised—only what the document means to Compose.
  Diffing generated output across `hllc` versions may show churn.
- **The Rust API of the `hl-*` crates.** Type layouts, public fields,
  error enum variants (none are `#[non_exhaustive]`), function
  signatures, module paths—all implementation detail, changeable at
  any time. Don't depend on these crates as libraries. Depend on the `hllc`
  binary and pin it to a tag.

Because the Rust API is deliberately outside the contract, this repo does
**not** run `cargo-semver-checks`: it would gate the one surface that's
explicitly not promised, and would fail on precisely the changes the
project wants to make freely.

Pre-1.0 (`0.x`), the same three covered surfaces are what `semver-*`
labels describe, but a breaking change is still just a minor bump, per
the usual 0.x convention. Crossing to 1.0.0 is a deliberate act, not an
arithmetic consequence of a `semver-major` label: `release.yml` refuses to
bump `0.x` to `1.0.0` unless the merged PR also carries a `release-1.0`
label. See CONTRIBUTING.md for how to pick a label.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the PR workflow, which
requires a linked issue and a semver label, and for the local checks to
run before requesting review.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
the [Massachusetts Institute of Technology (MIT) License](LICENSE-MIT) at
your option.
