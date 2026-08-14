# hl-lang

`hl-lang` is a small declarative DSL that transpiles to Docker Compose YAML
plus Traefik labels, so that standing up a new homelab service doesn't mean
rewriting a near-identical Compose block + label set every time. It's a
transpiler, not an interpreter — no evaluation, no closures, no runtime —
and doubles as a "learn to write a language" project covering the
lexer → parser → AST → codegen pipeline.

The full design (motivation, grammar, worked examples, open questions) lives
in the project vault at `1-Projects/Homelab Compose DSL.md`. This repo is the
implementation.

## Status

**Lexer milestone only.** The lexer (`crates/hl-lexer`) and a thin CLI stub
that prints its token stream (`crates/hl-cli`) exist; the parser, AST, and
codegen stages described in the design doc are not implemented yet.

## Layout

```
hl-lang/
  crates/
    hl-lexer/   # the lexer: source text -> Token stream
    hl-cli/     # `hl-cli <file.dsl>` — lexes a file and prints its tokens
```

## Building & testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Try the CLI against a `.dsl` file:

```sh
cargo run -p hl-cli -- crates/hl-lexer/tests/fixtures/jellyfin.dsl
```

## Lexical grammar

```
IDENT   ::= [A-Za-z_][A-Za-z0-9_-]*
NUMBER  ::= [0-9]+
STRING  ::= '"' [^"\n]* '"'
COMMENT ::= '#' [^\n]*        # to end of line; not part of the token stream

Reserved (not usable as IDENT): "template"
Punctuation: { } [ ] ( ) : = -> ,
```

- `template` is the *only* reserved word. Every other keyword-shaped
  identifier (`service`, `with`, `as`, `external`, `raw`, `defaults`, ...) is
  lexed as a plain `Ident` — the lexer has no notion of which identifiers are
  meaningful; that's resolved later by the parser against a schema table.
- `NUMBER` is integer-only: no sign, decimal point, or exponent.
- `STRING` has no escape sequences and cannot contain `"` or a newline; an
  unterminated string is a lex error.
- `->` is always a single token; a bare `-` is never valid on its own.
- `#` line comments are an addition made during the lexer milestone — the
  original grammar didn't define comment syntax at all. See the vault note's
  "Comments" section for the resolution.

See `crates/hl-lexer/src/lib.rs` for the full rustdoc, including token kinds,
span semantics, and error types.
