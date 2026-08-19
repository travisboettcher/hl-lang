# Syntax basics

This page explains how `.hll` files fit together, in plain terms.
If you've read [Getting Started](./getting-started.md) you've already seen
most of these shapes in practice—this page names them and explains
the rules behind them.

## Declarations

A `.hll` file is a sequence of top-level declarations. There are three
kinds:

- A **named declaration**—a `service`, a `network`, or a `volume`—gives
  a type and a name, followed by a body: `service jellyfin { ... }`.
- A **template declaration** starts with the word `template`, the *one*
  reserved word in the whole language (see [Reserved words](#reserved-words)
  below): `template internal_web(port: Number) { ... }`.
- A **`use` declaration** imports another file: `use "docker.hll" as
  traefik`. See [Imports](./imports.md).

## Bodies and statements

A body is a `{ }`-delimited list of statements, one per line:

```hll,build
service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.example.com"
  restart unless-stopped
}
```

Every statement is one of two shapes:

- `key: value`—an explicit field assignment, for example, `restart: unless-stopped`.
- `key` followed by some shorthand—the common case in practice, covered
  below.

A "value" itself can be a string (`"jellyfin/jellyfin:latest"`), a number
(`8096`), a bare word (`unless-stopped`), a list (`[a, b, c]`), or another
nested statement—bodies nest arbitrarily, which is how `expose { port:
8096, host: "..." }` and `with internal_web { port: 8080 }` both work: the
`{ ... }` after `internal_web` is itself a body, using the exact same
grammar as a service's own top-level body.

## Reserved words

`template` is the only word in `hll` that you can never use as an
identifier. Everything else that looks like a keyword—`service`,
`network`, `image`, `volume`, `env`, `restart`, `expose`, `middleware`,
`with`, `as`, `use`, `raw`, `defaults`, and so on—is an ordinary
identifier that only *means* something because of where it appears and
what field it's assigned to. This is deliberate: it keeps the door open
for a field or a template named almost anything, without a growing list
of words you have to avoid.

## The primary-value shorthand

Writing `expose { port: 8096 }` for a type that has one obvious "main"
field is more ceremony than the information deserves. Any type with a
**primary field** lets you skip the field name and the braces, and just
write the value directly after the type name:

```hll,fragment
image "jellyfin/jellyfin:latest"
# same as: image { ref: "jellyfin/jellyfin:latest" }

expose 8096
# same as: expose { port: 8096 }
```

`image`'s primary field is `ref`, and `expose`'s is `port`. See [Built-in
Fields](./built-in-fields.md) for the full list of which type's primary
field is which.

## Secondary-field shorthand

`expose` needs more than just a port in practice—it also needs the
hostname Traefik should route from. Rather than dropping back to the full
form (a canonical `{ }` body, each field on its own line—see [Layout
rules](#layout-rules) below), one specific shorthand lets a second field
fuse directly onto the primary value with no comma:

```hll,fragment
expose 8096 as "media.example.com"
# same as:
# expose {
#   port: 8096
#   host: "media.example.com"
# }
```

`as` is the one built-in case of this—it aliases onto `expose`'s `host`
field. It's a one-shot continuation, though, not a list: nothing else can
follow `as`, comma or no comma—`expose 8096 as "media.example.com",
entrypoint: web-secure` is a **compile error**. To set additional fields,
drop the `as` shorthand and name `host` explicitly instead. From there,
you can add further `key: value` fields, each preceded by a comma:

```hll,fragment
expose 8096, host: "media.example.com", entrypoint: web-secure
```

## Map-style shorthand

Three types—`volume`, `publish`, and `env`—are conceptually key/value
maps rather than named struct fields, and each has its own
natural-looking separator instead of a colon:

```hll,fragment
volume "/mnt/media" -> "/data"     # host path -> container path
publish 8096 -> 8096               # host port -> container port
env PUID = "1000"                  # key = value
```

`volume` here is the *field* that mounts something into a service. A
`volume` at the top level of a file, outside any service body, is a
different thing—the declaration of a named Docker volume, whose body is
an ordinary struct body. See [`volume`](./built-in-fields.md#volume).

Writing any of these more than once in the same body accumulates
entries rather than overwriting—a service can have several `volume`
lines, several `publish` lines, and several `env` lines. The same is
true of `middleware` and `depends_on`, which are list fields. `image`
and `restart`, by contrast, are scalar—writing either twice in the same
body is a compile error, not a silent overwrite.

## Layout rules

Two rules govern whitespace and punctuation, and both matter in practice:

- **Different fields go on different lines, not different fields separated
  by commas.** `image "x"` and `restart unless-stopped` must each be on
  their own line inside a service/template/network body. A comma between
  two unrelated fields (`image "x", restart unless-stopped`) is a compile
  error. A comma continues a *single* field's own list—it never marks the
  boundary between two different fields. (`volume`/
  `env`/`raw { }` bodies and a `with`-invocation's argument body are the
  exception—see below.)
- **A trailing comma continues a list, but its absence ends it.** This applies
  to bracket lists (`[a, b, c]`), a bare `with`-list (`with a, b, c`), and
  the preceding secondary-field shorthand. If there's a next item, the comma
  before it's mandatory—bare adjacency with no comma does *not* imply
  continuation.

```hll
volume syncthing-config {}

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
trailing comma—this parses identically to writing it all on one line.

`volume { }`/`env { }`/`raw { }` bodies, and a `with`-invocation's own
argument body (`{ port: 8080 }` from the preceding example), are the
exception to the newline rule: they're key/value maps, not named struct
fields, so the compact one-line style (`{ puid: 1000, pgid: 100 }`) is fine
there, comma-separated or one entry per line with no commas at all. What's
still not valid is bare adjacency *on one line* with neither—`{ "a": "/x"
"b": "/y" }` is a parse error, just written without the comma the
preceding struct-body rule would otherwise demand a newline for instead.

## Comments and interpolation

A `#` starts a line comment, running to the end of the line. It's only
recognized between tokens—a `#` inside a string is just a literal
character, not a comment:

```hll,build
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

Numbers are integers only—no sign, no decimal point, no exponent
(`8096`, not `8096.0` or `-1`). Strings are double-quoted, with no escape
sequences and no way to embed a literal `"` or a newline inside one.

A `template`'s declared parameter can optionally have type `Number` or
`String` (`template linuxserver_app(puid: Number, pgid: Number) { ... }`).
The check is strict, not coercive—a `Number` parameter rejects a quoted
string argument even if it looks numeric, and vice versa. An untyped
parameter accepts any literal.
