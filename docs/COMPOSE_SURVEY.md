# Compose feature survey

`hll`'s built-in schema deliberately covers a slice of Docker Compose,
and [`raw`](../book/src/built-in-fields.md#raw) covers the rest. This
document measures which slice, against 824 Compose files sampled from
public repositories on 2026-08-30, and ranks the keys `hll` lacks by how
much applicability each one buys.

The framing matters, because `raw` already reaches every Compose key
this survey found. What a prevalence number answers isn't "can `hll`
emit this" but "does this key appear often enough to deserve validation,
`{{name}}` interpolation, and template-level merging instead of a
passthrough escape hatch."

## Summary

- **Today `hll`'s built-ins fully express 53% of sampled services and
  34% of sampled files.** The other half of the services need at least
  one `raw` key.
- **Nine additions take file-level coverage from 34% to 90%**, in this
  order of marginal gain: `user`, `stop_grace_period`, `deploy`,
  `hostname`, `init`, `shm_size`, `network_mode`, `cap_add`, and
  `security_opt`.
- **`user` is the single biggest gap** at 27% of files, 20% of services,
  and present in four of the five corpora.
- **`deploy`'s prevalence is almost entirely one shape.** 80 of its 89
  uses are a GPU device reservation nested four levels deep. That argues
  for a `gpu` sugar field rather than a generic `deploy` struct.
- **`raw { labels: ... }` silently discards every generated Traefik
  label.** See "Defects this survey turned up," later in this document.
  For a language whose premise is Traefik labels, and whose users also
  run dashboard and updater labels, that's the most consequential
  finding here.
- **`${VAR}` passthrough already works** and no document says so.
- **The survey confirms several existing bets.** Nothing in the corpus
  uses Compose's long `ports` syntax, `extends` appears zero times, and
  YAML anchors appear in under 1% of files, so `hll`'s short-syntax
  `publish` and its `template`/`with` composition aren't leaving
  anything on the table.

## Corpus and method

| Source | Files | What it represents |
|---|---|---|
| [`docker/awesome-compose`](https://github.com/docker/awesome-compose) `30f4b7f` | 39 | Sample stacks from the Docker project, development-shaped |
| [`Haxxnet/Compose-Examples`](https://github.com/Haxxnet/Compose-Examples) `00c5ca2` | 168 | Hand-written homelab stacks behind Traefik |
| [`getumbrel/umbrel-apps`](https://github.com/getumbrel/umbrel-apps) `e08a7b4` | 391 | One self-hosting app store's manifests |
| [`linuxserver/docker-documentation`](https://github.com/linuxserver/docker-documentation) `4ff4454` | 208 | The published Compose snippet per LinuxServer.io image |
| 18 upstream project repositories | 18 | Each project's own reference file |
| **Total** | **824** | **1,749 services** |

Each file went through a YAML loader. The survey then tallied top-level
keys, per-service keys, and the shape of each value. Percentages against
files count a file once no matter how many of its services use a key,
and percentages against services count each service. A separate textual
pass caught what a loader erases: `${VAR}` interpolation, anchors, and
the comment convention described next.

Four biases are worth stating plainly:

- **The corpus over-weights two uniform populations.** The 208
  LinuxServer.io snippets are near-identical by construction, and the
  391 `umbrel-apps` manifests follow one app store's house style,
  notably `user: 1000:1000`, `stop_grace_period`, and `init: true`.
  Every ranking that follows therefore comes with a per-corpus
  breakdown, and the recommendations discount a key that only one
  corpus loves.
- **`Compose-Examples` comments out its Traefik labels and its proxy
  network**, leaving the reader to strip the `#`. The loader can't see
  those, so parsed `labels` and `networks` counts understate that corpus
  badly: 290 commented `networks:` keys and 118 commented `labels:` keys
  sit in files the parser scores as having neither. Reported label
  figures come from the textual pass instead.
- **Documentation snippets aren't deployments.** They show what a
  project recommends, not what a homelab ends up running.
- **This is one point in time**, and Compose's own defaults move.

## What `hll` expresses today

Built-in service fields: `image`, `build`, `expose`, `publish`,
`volume`, `env`, `env_file`, `restart`, `healthcheck`, `depends_on`,
`networks`, `dns`, `devices`, `privileged`, `container_name`, `command`,
and `entrypoint`, plus `router`/`traefik` for labels and `with` for
composition. Top-level: `network`, `volume`, `service`, `template`, and
`use`.

Scoring a service as expressible when every key it sets maps to one of
those, and counting a `labels` block as expressible only when every
label starts with `traefik.`, gives the baseline:

| | Expressible today |
|---|---|
| Services | 928 of 1,749, or 53.1% |
| Files | 280 of 824, or 34.0% |

## Findings

### Service-level key prevalence

| Key | Files | Services | In `hll` |
|---|---|---|---|
| `image` | 98.9% | 76.6% | yes |
| `environment` | 93.8% | 83.4% | yes |
| `restart` | 86.3% | 69.3% | yes |
| `volumes` | 81.7% | 59.6% | yes |
| `ports` | 47.9% | 24.9% | yes |
| `container_name` | 38.3% | 26.5% | yes |
| `user` | 27.1% | 20.3% | **no** |
| `depends_on` | 24.8% | 19.4% | yes |
| `stop_grace_period` | 22.5% | 13.6% | **no** |
| `expose` | 20.1% | 13.1% | yes |
| `command` | 16.0% | 12.3% | yes |
| `healthcheck` | 13.2% | 12.8% | yes |
| `deploy` | 10.6% | 5.1% | **no** |
| `hostname` | 8.6% | 5.4% | **no** |
| `init` | 5.5% | 3.3% | **no** |
| `networks` | 5.2% | 6.5% | yes |
| `network_mode` | 5.1% | 2.6% | **no** |
| `entrypoint` | 4.6% | 3.4% | yes |
| `shm_size` | 3.8% | 1.8% | **no** |
| `build` | 3.5% | 3.3% | yes |
| `cap_add` | 3.5% | 1.7% | **no** |
| `env_file` | 3.0% | 3.4% | yes |
| `security_opt` | 2.7% | 1.4% | **no** |
| `privileged` | 2.2% | 1.0% | yes |
| `read_only` | 1.7% | 0.9% | **no** |
| `labels` | 1.3% | 1.1% | Traefik only |
| `ulimits` | 1.3% | 0.9% | **no** |
| `extra_hosts` | 1.3% | 0.7% | **no** |
| `devices` | 1.3% | 0.6% | yes |
| `tmpfs` | 1.1% | 0.6% | **no** |
| `secrets` | 1.0% | 0.9% | **no** |

Below 1%: `links`, `stop_signal`, `sysctls`, `cap_drop`, `mem_limit`,
`dns`, `tty`, `stdin_open`, `platform`, `logging`, `working_dir`,
`pull_policy`, `runtime`, `pids_limit`, `configs`, `memswap_limit`,
`device_cgroup_rules`, `mac_address`, and `profiles`.

### Marginal coverage

Adding one field at a time, each time picking the field that unblocks
the most files no other missing field blocks:

| Add | Cumulative file coverage |
|---|---|
| baseline | 34.0% |
| `user` | 44.9% |
| `stop_grace_period` | 60.2% |
| `deploy` | 70.3% |
| `hostname` | 76.5% |
| `init` | 81.2% |
| `shm_size` | 84.0% |
| `network_mode` | 86.5% |
| `cap_add` | 88.6% |
| `security_opt` | 89.8% |
| `ulimits` | 90.8% |

Per corpus, the same greedy ordering diverges sharply, which is the
strongest argument against reading the combined table too literally:

| Corpus | Files | Coverage today | First three additions |
|---|---|---|---|
| `awesome-compose` | 39 | 51% | `stop_signal`, `stdin_open`, `cap_add` |
| homelab, `Compose-Examples` | 168 | 49% | `hostname`, `user`, `links` |
| `umbrel-apps` | 391 | 19% | `user`, `stop_grace_period`, `init` |
| LinuxServer.io docs | 208 | 46% | `deploy`, `shm_size`, `hostname` |
| upstream projects | 18 | 39% | `user`, `shm_size`, `links` |

`hostname` leads the homelab corpus at 32% of files. `stop_grace_period`
ranks second overall purely on the 46% it reaches in `umbrel-apps`, and
sits at 1% or less everywhere else.

### Value shapes

The shapes matter as much as the keys, because they say whether `hll`'s
existing spellings are the right ones.

**Ports.** 638 entries, and not one uses Compose's long mapping syntax.
138 of them, 21.6%, carry a `/tcp` or `/udp` suffix, 18 pin a host
interface, 7 are ranges, and 35 interpolate a variable. `publish`'s
"write both sides exactly as you'd write them in Compose's short syntax"
already handles all four.

**Volumes.** 1,714 entries, 12 of them long syntax. 1,610, or 93.9%, are
bind mounts and 104 point at a named volume, so `volume`'s arrow map,
whose host side is a path or a declared volume reference, is the right
way round. 249 entries, 14.5%, end in `:ro`, which `read_only` covers.
51 write an explicit `:rw`, and 20 use the SELinux `:z` and `:Z`
relabeling flags that `docs/DESIGN.md` already defers.

**Environment.** 901 services use the mapping form and 557 the list
form. `env k = v` renders to the list form and reads like the mapping
one, so neither population is fighting the syntax. Of 7,219 entries,
1,060, or 14.7%, reference a `${VAR}`.

**Interpolation.** 529 files, 64.2%, contain `${VAR}`, and 173, 21.0%,
use the `${VAR:-default}` form. That makes interpolation the most
prevalent single feature in the corpus after `image` itself.

**`depends_on`.** 143 services use the mapping form against 197 plain
lists, and the conditions break down as `service_healthy` 248,
`service_started` 117, and `service_completed_successfully` 8. `hll`
already accepts all three.

**`healthcheck`.** `test` as a list beats a string 185 to 32, and the
sub-key ranking runs `interval` 198, `timeout` 195, `retries` 194,
`start_period` 137, `disable` 7, and `start_interval` 1. `hll`'s field
list matches exactly.

**`restart`.** `on-failure` 648, `unless-stopped` 436, `always` 125, and
`no` 1. The `on-failure` lead is the `umbrel-apps` house style, and
`unless-stopped` leads everywhere else.

**`user`.** 333 of 355 uses are the literal `1000:1000`.

**`deploy`.** 86 of 89 uses set only `resources`, 83 of those set
`reservations`, and 80 of those are a GPU device reservation. `limits`
appears 6 times.

**`cap_add`.** `NET_ADMIN` 26, `SYS_MODULE` 7, `NET_RAW` 7, and
`SYS_ADMIN` 3, which is mostly the virtual-private-network sidecar
pattern.

**`security_opt`.** `no-new-privileges:true` accounts for 16 of 24.

**`network_mode`.** `host` accounts for 40 of 46.

**Images.** 679 references pin a digest, 337 float on `:latest`, 221
name an explicit tag, and 103 carry no tag. `image`'s opaque string
handles all four.

**Multi-service files.** 39.6% of files declare one service, 35.6%
declare two, and 24.8% declare three or more, topping out at 56. A
file-per-service model would have been the wrong shape, and `hll`
doesn't assume one.

### Where the corpus confirms existing choices

- **Dropping `version`.** 44.1% of files still write the obsolete
  top-level `version` key. `hll` emits none, which matches the current
  Compose specification and quietly fixes a stale habit in nearly half
  the corpus.
- **`template`/`with` over the reuse YAML offers.** `extends` appears in
  **zero** files. YAML anchors appear in 7 files, merge keys in 6, and
  `x-` extension fields in 5, each under 1%. Real Compose files don't
  factor out their own repetition. They copy and paste, which is exactly
  the repetition `hl-lang` exists to remove. The near-absence of anchors
  isn't evidence that nobody wants reuse. It's evidence that the YAML
  answer is too awkward to reach for.
- **Traefik as the routing target.** 115 files, 14.0%, carry Traefik
  labels, 881 occurrences in all, and the homelab corpus revolves around
  them. No competing reverse-proxy label vocabulary shows up at
  comparable scale.
- **Named volumes as a top-level declaration.** Only 4.4% of files
  declare top-level `volumes:` and 2.9% `networks:`, matching the
  bind-mount-dominant picture. Where they do appear, the sub-keys are
  `driver` 15, `external` 9, `internal` 8, `ipam` 1, and `driver_opts`
  1. `hll` already has all but `internal` and `ipam`.

## Defects this survey turned up

### `raw { labels: ... }` silently discards generated Traefik labels

Combining any non-Traefik label with a `router` produces a service with
no Traefik labels at all, no diagnostic, and exit status 0:

```hll
network proxy { external }
service app {
  image "nginx:latest"
  networks [proxy]
  expose 80
  router web { host: "app.example.com" }
  raw {
    labels: ["homepage.group=Media"]
  }
}
```

```yaml
services:
  app:
    image: nginx:latest
    networks:
    - proxy
    expose:
    - 80
    labels:
    - homepage.group=Media
```

The three `traefik.*` labels the same file emits without the `raw` block
have vanished. Users routinely combine Traefik routing with dashboard
labels such as `homepage.*` and updater labels such as
`com.centurylinklabs.watchtower.enable`, and the corpus carries both
vocabularies alongside Traefik. Whatever the fix turns out to be, be it
merging the two label sources, rejecting the combination, or a
first-class `labels` field that merges, silently dropping the routing
the language exists to generate is the wrong behavior. This deserves an
issue of its own, ahead of any new field.

### `${VAR}` passthrough works, and nothing documents it

Compose-level interpolation survives codegen untouched, because `hll`
resolves only `{{name}}` and treats `$` as a sigil solely at the start
of a template parameter reference outside a string:

```hll
service probe {
  image "linuxserver/sonarr:${TAG}"
  env TZ = "${TZ:-Etc/UTC}"
  volume "${DOCKER_VOLUME_STORAGE:-/mnt/docker-volumes}/sonarr" -> "/config"
  publish "127.0.0.1:8989" -> "8989/tcp"
}
```

Every one of those reaches the YAML verbatim. Given that 64% of the
corpus interpolates, `book/src/syntax-basics.md` should say so next to
`{{name}}`, and a golden test should pin the behavior so a future
escaping change can't break it by accident.

## Recommendations

Each candidate that follows passes the test `docs/DESIGN.md` sets for
itself: "would this make sense on a homelab with completely different
infrastructure?" Each one is a plain Compose key with no homelab
convention baked in.
The homelab-specific *bundles* they enable, such as a hardening template
pairing `security_opt`, `cap_drop`, and `read_only`, stay in template
files where that document puts them.

### Tier 1: implement

Ordered by combined prevalence and breadth across corpora, not by the
greedy table alone.

| Field | Why | Sketch |
|---|---|---|
| `user` | 27% of files, four of five corpora | `user "1000:1000"`, a struct with a primary field taking a string or a number |
| `hostname` | 32% of the homelab corpus, present in all five | `hostname "{{name}}"`, where `{{name}}` earns its keep |
| `cap_add`, `cap_drop` | The sidecar and hardening patterns | `cap_add [NET_ADMIN]`, a reference list of bare identifiers |
| `security_opt` | 2.7%, and `no-new-privileges:true` dominates it | `security_opt ["no-new-privileges:true"]`, a string list |
| `init` | 5.5% of files, three corpora | `init`, a bare flag alongside `privileged` |
| `network_mode` | 5.1%, and `host` conflicts with `publish` | `network_mode host`—bare word or string |
| `gpu` | 80 of 89 `deploy` uses, four levels of nesting | see the note that follows |
| `labels` | Small in the parse, blocked by the defect | `labels { "homepage.group": "Media" }`, merging with generated Traefik labels |
| `stop_grace_period`, `stop_signal` | Cheap scalars, and one corpus leans hard on the first | `stop_grace_period "1m"` |
| `shm_size` | 3.8%, and database images need it | `shm_size "1gb"` |

**On `gpu` rather than `deploy`.** The generic option is a nested
`deploy` struct mirroring Compose. The corpus argues against it, because
`deploy` in practice is one shape, and reproducing four levels of
nesting to reach it would make the most verbose block in the language
one of its rarest. A `gpu` field collapses it:

```hll
service jellyfin {
  image "linuxserver/jellyfin:latest"
  gpu { driver: nvidia, count: 1, capabilities: [compute, video, utility] }
}
```

That's the same move `router` already makes over Traefik labels, sugar
over boilerplate whose expansion is plain Compose, so it fits the
language's existing style rather than straining it. Every other use of
`deploy` stays with `raw`, where the 6 occurrences of `resources.limits`
belong.

**On `network_mode`.** Setting `host` makes `publish` and `networks`
meaningless, which is a diagnostic `hll` is well placed to emit and
Compose itself doesn't. The `service:<name>` form, the
virtual-private-network sidecar pattern, could later resolve against a
declared service the way `networks` resolves against a declared network.
That's worth designing, and not worth blocking the string form on.

### Tier 2: worth having, low urgency

`read_only`, `tmpfs`, `ulimits`, `extra_hosts`, `sysctls`, `platform`,
`pull_policy`, `working_dir`, and memory and processor limits. Each sits
at or under 1.7% of files. `read_only` and `tmpfs` pair naturally with
the Tier 1 hardening fields and could ship with them.

### Deliberately out of scope

- **`version`.** Obsolete. Keep emitting nothing.
- **`extends` and YAML anchors.** Zero and 0.8%. `template`/`with`
  already covers the need better.
- **`secrets` and `configs`.** 1.0% and 0.1%, concentrated in
  `awesome-compose`, and Swarm-shaped. `raw` is the right home.
- **`profiles`.** One file. `raw`.
- **`links`.** Deprecated in Compose. It surfaces in the greedy tables
  only as an artifact of old files.
- **`logging`.** 3 files, all `json-file`, which is the default anyway.
- **The per-service network mapping form** with `ipv4_address`: 44
  services, 40 of them `umbrel-apps` manifests assigning static
  addresses from a platform-managed range. Not a general homelab need.

## Reproducing this survey

Clone the four repositories at the commits in the corpus table, collect
every `docker-compose*.y*ml` and `compose*.y*ml` outside `.git`, extract
the first fenced YAML block containing `services:` from each
`docs/images/docker-*.md` in the LinuxServer.io documentation, add each
upstream project's own reference file, then tally per-file and
per-service key occurrences with a YAML loader plus a textual pass for
`${VAR}`, anchors, and commented-out directives. The counts here name
their denominators throughout, so a rerun on a later snapshot compares
directly.
