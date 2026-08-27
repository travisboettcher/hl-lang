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
  below): `template internal_web(port) { ... }`.
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
nested statement—bodies nest arbitrarily, which is how `router { host:
"...", entrypoint: web-secure }` and `with internal_web { port: 8080 }`
both work: the `{ ... }` after `internal_web` is itself a body, using
the exact same grammar as a service's own top-level body.

## Reserved words

`template` is the only word in `hll` that you can never use as an
identifier. Everything else that looks like a keyword—`service`,
`network`, `image`, `build`, `volume`, `env`, `restart`, `expose`,
`router`,
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

A type with several fields—`router`, most often, since a router usually
needs a `host` and sometimes an `entrypoint` or a `path_prefix` too—lets
you skip the full `{ }` body (each field on its own line—see [Layout
rules](#layout-rules) below) and instead fuse further fields onto the
primary position with a leading comma:

```hll,fragment
router api, host: "media.example.com", entrypoint: web-secure
# same as:
# router api {
#   host: "media.example.com"
#   entrypoint: web-secure
# }
```

From there, you can keep adding further `key: value` fields, each
preceded by a comma.

`expose <port> as "<host>"` looks similar but is a different mechanism—a
bespoke, one-shot spelling that desugars to `expose { port }` plus an
unnamed `router { host }` (see [Built-in
Fields](./built-in-fields.md#expose)), not a comma-continued field list.
`as` fuses onto the primary value and stops there: nothing else can
follow it, comma or no comma—`expose 8096 as "media.example.com",
entrypoint: web-secure` is a **compile error**. A service that needs
more than a bare host writes `router` out explicitly instead:

```hll,fragment
expose 8096
router {
  host: "media.example.com"
  entrypoint: web-secure
}
```

## Map-style shorthand

Three types—`volume`, `publish`, and `env`—are conceptually key/value
maps rather than named struct fields, and each has its own
natural-looking separator instead of a colon:

```hll,fragment
volume "/mnt/media" -> "/data"     # host path -> container path
volume media -> "/media"           # named volume -> container path
publish 8096 -> 8096               # host port -> container port
env PUID = "1000"                  # key = value
```

`volume` is the one map-style field whose key side can be either. A
quoted host is a path. An unquoted one is an identifier referring to a
named Docker volume, and needs a declaration to refer to.

`volume` here is also the *field* that mounts something into a service.
A `volume` at the top level of a file, outside any service body, is a
different thing—the declaration of a named Docker volume, whose body is
an ordinary struct body. See [`volume`](./built-in-fields.md#volume).

Writing any of these more than once in the same body accumulates
entries rather than overwriting—a service can have several `volume`
lines, several `publish` lines, and several `env` lines. The same is
true of `networks` and `depends_on`, which are list fields. `image`
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
  volume syncthing-config -> "/config"
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
template internal_web(port) {
  expose $port
  router { host: "{{name}}.internal.example.com" }}
```

Applied inside `service syncthing { with internal_web { port: 8384 } }`,
`{{name}}` resolves to `syncthing`, producing
`syncthing.internal.example.com`.

## Numbers and strings

Numbers are integers only—no sign, no decimal point, no exponent
(`8096`, not `8096.0` or `-1`). Strings are double-quoted, and a
backslash escapes the character after it:

| Escape | Character |
| ------ | --------- |
| `\"`   | a double quote |
| `\\`   | a backslash |
| `\n`   | a newline |
| `\t`   | a tab |
| `\r`   | a carriage return |

Those five are the whole set. A backslash followed by anything else, such
as `"\q"`, is a compile error naming the backslash, so an escape the
language doesn't have never turns into the two characters that spell it.

Use them wherever a value needs a quote or a line break of its own—a
shell command that quotes its own argument, a JSON blob in an
environment variable, or the multi-line `entrypoint` a `raw` block passes
straight through to Compose:

```hll,fragment
command "sh -c \"exec nginx -g 'daemon off;'\""
env CONFIG = "{\"log\": \"debug\"}"
raw {
  entrypoint: "echo starting\nexec /app/server"
}
```

A string still can't run past the end of its line: `\n` is how you write
a newline, and a line that ends before its closing `"` is a compile
error. So is a string ending in a backslash, such as `"C:\"`—that
backslash escapes the closing quote, which leaves the string unfinished.
Write a trailing backslash as `\\`.

A value that lands in a Traefik label—a [`router`](./built-in-fields.md#router)'s
`host`, plus each `entrypoint` entry—rejects a newline or a tab, on top
of the metacharacter set it already rejects. Neither one belongs in a
hostname or an entry point name, and either one changes what the
generated label means.

A `template`'s declared parameter carries no type annotation—just a bare
name (`template linuxserver_app(puid, pgid) { ... }`). Instead,
composition checks a substituted argument against the field it lands in:
a reference-shaped position such as `networks` or a router's
`middleware` rejects a
bare number, since that position's own grammar can never hold one
directly, and a `number`-typed position such as `expose.port` rejects
anything that isn't one, whether the value arrives through a `$param` or
you write it directly—so `expose "eight-thousand"` fails the same way
`with a_template { port: "eight-thousand" }` would. Every other
position accepts any literal kind, exactly as writing it directly would.
