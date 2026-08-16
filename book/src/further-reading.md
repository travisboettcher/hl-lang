# Further Reading

This book covers `hll` from a user's point of view — enough to write and
compile real `.hll` files for your own homelab. A few things live
elsewhere:

- [`docs/DESIGN.md`](https://github.com/travisboettcher/hl-lang/blob/main/docs/DESIGN.md) —
  the formal spec: the lexical/syntactic grammar in BNF, the desugaring
  rules, and the full built-in schema table. This is the source of truth
  the lexer, parser, and codegen are built against, and the right place
  to check when this book's prose leaves an edge case ambiguous.
- Each crate's own rustdoc (`crates/hl-lexer/src/lib.rs`,
  `crates/hl-parser/src/lib.rs`, `crates/hl-linker/src/lib.rs`,
  `crates/hl-codegen/src/lib.rs`) — implementation details: token/AST
  shapes, span semantics, error types. Relevant if you're modifying the
  compiler itself rather than writing `.hll` files.
- The [repository README](https://github.com/travisboettcher/hl-lang#readme) —
  installing a released `hllc` binary, building from source, running the
  test suite, and how releases are cut.
- [`CONTRIBUTING.md`](https://github.com/travisboettcher/hl-lang/blob/main/CONTRIBUTING.md) —
  the PR workflow, if you'd like to contribute to `hll` itself.

Found a gap in this book, or something that's out of date with the
compiler's actual behavior? [Open an issue](https://github.com/travisboettcher/hl-lang/issues/new).
