# Contributing to hl-lang

## Before opening a PR

1. **File or find an issue first.** Every PR into `main` must be linked to
   an issue — put `Closes #123` (or `Fixes`/`Resolves #123`) in the PR
   description, or link one manually via the "Development" panel on the
   PR. A required check enforces this, so a PR with no linked issue won't
   be mergeable.
2. **Branch off `main`** and make your changes there.
3. Your PR must carry exactly one of the `semver-major` / `semver-minor` /
   `semver-patch` labels — this drives the automated release (see
   "Releasing" in the README). See "Picking a semver label" below; a
   maintainer can help pick if it's unclear.

## Picking a semver label

The label describes compatibility for **users of `hllc`**, not for
callers of the `hl-*` crates as Rust libraries. Those crates are
implementation detail of the compiler and carry no API guarantee — see
"What a version number promises" in the README. Concretely, ask these
three questions about your change:

1. Does it break an existing `.hll` file — something that compiled
   before and now doesn't, or now compiles to something different?
2. Does it break an existing `hllc` invocation — a removed or renamed
   flag, a changed flag meaning, a changed exit code, output moved
   between stdout and stderr?
3. Does it change the meaning of previously-generated output — the
   emitted Compose YAML now describes different behavior when
   `docker compose up` runs it (different image, ports, volumes,
   networks, environment, Traefik routing)?

- **`semver-major`** — yes to any of the three. Post-1.0 this is a real
  breaking release. Pre-1.0 (`0.x`) prefer `semver-minor` for a breaking
  change, per the usual 0.x convention: a `semver-major` label on `0.x`
  ships **1.0.0**, and `release.yml` deliberately refuses to do that
  without an additional `release-1.0` label on the same PR (see "Crossing
  to 1.0" below).
- **`semver-minor`** — no to all three, but the change adds something
  users can reach: new syntax, a new built-in field, a new CLI flag, new
  generated-output capability. Pre-1.0, this is also the label for a
  deliberate breaking change that should stay within `0.x`.
- **`semver-patch`** — no to all three and nothing new is exposed: bug
  fixes, performance work, refactors, docs, tests, CI, dependency bumps.

Changes to things the version number explicitly does **not** cover are
`semver-patch` even when they're user-visible: exact diagnostic wording,
exact YAML key ordering or formatting, and anything about the crates'
Rust API (adding an error enum variant, adding a struct field, changing
a signature). None of those are promised, so none of them force a bump.

### Crossing to 1.0

`release.yml` computes the next version arithmetically from the label, so
on `0.x` a routine `semver-major` label would silently ship `1.0.0`. That
bump is gated: the job fails unless the merged PR also carries a
`release-1.0` label. Declaring 1.0 is a deliberate decision about the
`.hll` language and the `hllc` CLI being frozen (see the README), so it
takes two labels, not one.

## Before requesting review

Run the same checks CI runs:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Coverage is gated at 80% workspace-wide line coverage. CI runs it as

```sh
cargo llvm-cov --workspace --html --fail-under-lines 80
```

which is also what replaces the plain `cargo test --workspace` step
there. Note that `cargo-llvm-cov` skips doctests, which is why
`cargo test --workspace --doc` is listed separately above and run as its
own CI step. If you're adding a new code path, add tests alongside it
rather than relying on existing coverage margin.

If you're touching the lexer, parser, or codegen, consider whether the
fuzz targets in `fuzz/` need a new corpus seed or cover the change
already — see the README's "Building & testing" section for how to run
them locally.

## Scope

Keep PRs focused on one issue's worth of change. Unrelated formatting or
refactoring makes review harder and can hide the actual fix — open a
separate issue/PR for cleanup that isn't required by the change at hand.

## Design background

`docs/DESIGN.md` covers the language's motivation, grammar, and worked
examples; each crate's rustdoc (`crates/*/src/lib.rs`) covers
implementation details for that crate. Skim the relevant one before
making non-trivial changes to the lexer/parser/codegen pipeline.

`book/` is the user-facing guide published at
<https://travisboettcher.github.io/hl-lang/> (via `.github/workflows/docs.yml`,
mdBook). If a change adds or alters user-visible syntax or a built-in
field, update the relevant `book/src/*.md` page alongside `docs/DESIGN.md`
rather than letting the two drift apart. Build it locally with `mdbook
build book` (`cargo install mdbook`) or `mdbook serve book` to preview.

Every ` ```hll ` code block in `book/src/*.md` is compiled by
`crates/hl-cli/tests/book_examples.rs` as part of `cargo test
--workspace` — the same idea as a rustdoc doctest, so an example that
stops matching the real grammar fails CI instead of silently going stale
in the published book. Tag each fenced block via its info string (see
that test file's own doc comment for the full list): default
(` ```hll `) parses it as a standalone file; `,fragment` wraps a bare
statement in a throwaway `service { }` first; `,build` also runs the
full link/compose/codegen pipeline, for a complete worked example;
`,file=NAME,group=ID[,entry]` links multiple blocks together as one
multi-file `use` example; `,ignore` excludes the block from validation
entirely, for a snippet that's deliberately invalid (illustrating an
error message, say). A new example needs one of these, not just prose
describing it — and reach for `,ignore` only when the block genuinely
can't compile, since an ignored block is exactly the kind that rots.

## License

By contributing, you agree your contribution is licensed under the same
dual MIT/Apache-2.0 terms as the rest of the project (see `LICENSE-MIT`
and `LICENSE-APACHE`).
