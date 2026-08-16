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
   "Releasing" in the README). Add whichever matches the size of the
   change; a maintainer can help pick if it's unclear.

## Before requesting review

Run the same checks CI runs:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Coverage is gated at 80% workspace-wide line coverage
(`cargo llvm-cov --workspace --fail-under-lines 80`); if you're adding a
new code path, add tests alongside it rather than relying on existing
coverage margin.

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
multi-file `use` example. A new example needs one of these, not just
prose describing it.

## License

By contributing, you agree your contribution is licensed under the same
dual MIT/Apache-2.0 terms as the rest of the project (see `LICENSE-MIT`
and `LICENSE-APACHE`).
