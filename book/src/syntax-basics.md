# Syntax Basics

This chapter explains how `.hll` files are put together, in plain terms.
If you've read [Getting Started](./getting-started.md) you've already seen
most of these shapes in practice — this chapter names them and explains
the rules behind them.

## Declarations

A `.hll` file is a sequence of top-level declarations. There are three
kinds:

- A **named declaration** — a `service` or a `network` — gives a type and
  a name, followed by a body: `service jellyfin { ... }`.
- A **template declaration** starts with the word `template`, the *one*
  reserved word in the whole language (see [Reserved words](#reserved-words)
  below): `template internal_web(port: Number) { ... }`.
- A **`use` declaration** imports another file: `use "docker.hll" as
  traefik`. See [Imports](./imports.md).

## Bodies and statements

A body is a `{ }`-delimited list of statements, one per line:

```hll
service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.example.com"
  restart unless-stopped
}
```

Every statement is one of two shapes:

- `key: value` — an explicit field assignment, e.g. `restart: unless-stopped`.
- `key` followed by some shorthand — the common case in practice, covered
  below.

A "value" itself can be a string (`"jellyfin/jellyfin:latest"`), a number
(`8096`), a bare word (`unless-stopped`), a list (`[a, b, c]`), or another
nested statement — bodies nest arbitrarily, which is how `expose { port:
8096, host: "..." }` and `with internal_web { port: 8080 }` both work: the
`{ ... }` after `internal_web` is itself a body, using the exact same
grammar as a service's own top-level body.

## Reserved words

`template` is the only word in `hll` that can never be used as an
identifier. Everything else that looks like a keyword — `service`,
`network`, `image`, `volume`, `env`, `restart`, `expose`, `middleware`,
`with`, `as`, `use`, `raw`, `defaults`, and so on — is an ordinary
identifier that only *means* something because of where it appears and
what field it's assigned to. This is deliberate: it keeps the door open
for a field or a template named almost anything, without a growing list
of words you have to avoid.

## The primary-value shorthand

Writing `expose { port: 8096 }` for a type that has one obvious "main"
field is more ceremony than the information deserves. Any type with a
**primary field** lets you skip the field name and the braces, and just
write the value directly after the type name:

```hll
image "jellyfin/jellyfin:latest"
# same as: image { ref: "jellyfin/jellyfin:latest" }

expose 8096
# same as: expose { port: 8096 }
```

`image`'s primary field is `ref`; `expose`'s is `port`. See [Built-in
Fields](./built-in-fields.md) for the full list of which type's primary
field is which.

## Secondary-field shorthand

`expose` needs more than just a port in practice — it also needs the
hostname Traefik should route from. Rather than dropping back to the full
`{ port: 8096, host: "..." }` form, one specific shorthand lets a second
field fuse directly onto the primary value with no comma:

```hll
expose 8096 as "media.example.com"
# same as: expose { port: 8096, host: "media.example.com" }
```

`as` is the one built-in case of this — it aliases onto `expose`'s `host`
field. It's a one-shot continuation, though, not a list: `as` can't
itself be followed by anything else, comma or no comma —
`expose 8096 as "media.example.com", entrypoint: "web-secure"` is a
**compile error**. To set additional fields, drop the `as` shorthand and
name `host` explicitly instead; from there, further `key: value` fields
are allowed, each one preceded by a comma:

```hll
expose 8096, host: "media.example.com", entrypoint: "web-secure"
```

## Map-style shorthand

Two types — `volume` and `env` — are conceptually key/value maps rather
than named struct fields, and each has its own natural-looking separator
instead of a colon:

```hll
volume "/mnt/media" -> "/data"     # host path -> container path
env PUID = "1000"                  # key = value
```

Writing either of these more than once in the same body accumulates
entries rather than overwriting — a service can have several `volume`
lines and several `env` lines. The same is true of `middleware` and
`depends_on`, which are list fields. `image` and `restart`, by contrast,
are scalar — writing either twice in the same body is a compile error,
not a silent overwrite.

## Layout rules

Two rules govern whitespace and punctuation, and both matter in practice:

- **Different fields go on different lines, not different fields separated
  by commas.** `image "x"` and `restart unless-stopped` must each be on
  their own line inside a service/template/network body; a comma between
  two unrelated fields (`image "x", restart unless-stopped`) is a compile
  error. A comma is reserved for continuing a *single* field's own list —
  never for marking the boundary between two different fields. (`raw { }`
  bodies and a `with`-invocation's argument body are the one exception —
  see below.)
- **A trailing comma continues a list; its absence ends it.** This applies
  to bracket lists (`[a, b, c]`), a bare `with`-list (`with a, b, c`), and
  the secondary-field shorthand above. If there's a next item, the comma
  before it is mandatory — bare adjacency with no comma does *not* imply
  continuation.

```hll
service syncthing {
  with internal_web { port: 8384 },
       authenticated,
       linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}
```

A long `with` list reads better wrapped across multiple lines, one
template per line, as long as every line but the last ends with a
trailing comma — this parses identically to writing it all on one line.

`raw { }` bodies, and a `with`-invocation's own argument body (`{ port:
8080 }` above), are the one exception to the newline rule: they're
key/value maps, not named struct fields, so the compact one-line style
(`{ puid: 1000, pgid: 100 }`) is fine there.

## Comments and interpolation

A `#` starts a line comment, running to the end of the line. It's only
recognized between tokens — a `#` inside a string is just a literal
character, not a comment:

```hll
# Media server
service jellyfin {
  image "jellyfin/jellyfin:latest"  # pin this before upgrading
}
```

A string can contain `{{name}}`, which interpolates the enclosing
service's own name at compile time. This is how a template can generate a
per-service hostname without knowing the service's name in advance:

```hll
template internal_web(port: Number) {
  expose $port, host: "{{name}}.internal.example.com"
}
```

Applied inside `service syncthing { with internal_web { port: 8384 } }`,
`{{name}}` resolves to `syncthing`, producing
`syncthing.internal.example.com`.

## Numbers and strings

Numbers are integers only — no sign, no decimal point, no exponent
(`8096`, not `8096.0` or `-1`). Strings are double-quoted, with no escape
sequences and no way to embed a literal `"` or a newline inside one.

A `template`'s declared parameter can optionally be typed `Number` or
`String` (`template linuxserver_app(puid: Number, pgid: Number) { ... }`).
The check is strict, not coercive — a `Number` parameter rejects a quoted
string argument even if it looks numeric, and vice versa. An untyped
parameter accepts any literal.
