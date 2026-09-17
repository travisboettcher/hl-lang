//! The label keys `std:traefik` can emit, checked against a list kept
//! by hand.
//!
//! # What verifies this module, and what doesn't
//!
//! `std_traefik_output.rs` beside this file pins the module's output.
//! It began as an equivalence gate — the same service compiled through
//! the built-ins and through the module, asserted equal byte for byte —
//! and #271 deleted the side it compared against. What it does now is
//! assert that the module emits what it emitted yesterday, which is
//! worth having and is not a correctness check: a template whose key
//! was wrong from the first commit passes it forever, and the failure
//! is silent, a router Traefik never routes to.
//!
//! Nothing else in the repo covers that gap either. The
//! `docker compose config` differential grades a document against
//! Compose's parser, and Compose accepts any `labels` list whatsoever —
//! a label is a string to it. The TCP set is the sharpest case: nine
//! templates that are the HTTP set with one path segment changed, where
//! a typo looks exactly like a correct key to a snapshot.
//!
//! So this file is option 3 of #302, and the decision that went with
//! it, recorded in docs/DESIGN.md's "Modules bundled with the
//! compiler": the module's *keys* are reviewed against Traefik's
//! documentation here, its *output* is pinned next door, and its
//! *semantics* are verified by running it — #300's soak, not a test in
//! this repo. Standing Traefik itself up against a generated document
//! would close the last of those and is deliberately not done: it
//! reintroduces the third-party coupling #271 removed, and it is worth
//! its cost only once a real routing bug gets past the other two.
//!
//! What this catches is a misspelled or invented key — including one
//! that arrives as a copy-paste between the HTTP and TCP sets. What it
//! cannot catch is a key spelled right and used wrong. Adding or
//! changing a key means editing [`DOCUMENTED_KEYS`] in the same commit,
//! which is the point: a key change becomes a reviewable diff citing
//! Traefik's docs rather than a snapshot refresh.

use std::collections::BTreeSet;

use hl_parser::{LabelEntry, TopDecl};

/// The module's own bytes — the same file `hl_linker`'s `stdlib`
/// `include_str!`s into the binary, so this can't drift from what a
/// `use "std:traefik"` actually resolves to.
const MODULE: &str = include_str!("../../hl-linker/src/stdlib/traefik.hll");

/// Every label key `std:traefik` can write, with each interpolation
/// hole rendered as `<hole>`.
///
/// Checked against Traefik's own reference for the Docker provider,
/// <https://doc.traefik.io/traefik/routing/providers/docker/>, which is
/// the document that says what each key means. A key here is a key
/// Traefik reads; a key not here is one this module has no business
/// writing.
///
/// The `<hole>` rendering is deliberate. It collapses `{{router}}`,
/// `{{name}}` and any other interpolation to one placeholder, so this
/// list reads as the *Traefik* keys rather than as the module's
/// parameter names — two templates naming the same key through
/// different parameters are one entry, and renaming a parameter doesn't
/// churn the inventory. Which template writes which key, and with whose
/// name in the hole, is `std_traefik_output.rs`'s question.
const DOCUMENTED_KEYS: &[&str] = &[
    // Service-wide. `enable` is the provider's opt-out switch, and
    // `docker.network` picks which of a multi-homed container's
    // networks Traefik connects on.
    "traefik.docker.network",
    "traefik.enable",
    // HTTP routers: the four a router may carry, plus the pointer to a
    // Traefik service of its own.
    "traefik.http.routers.<hole>.entrypoints",
    "traefik.http.routers.<hole>.middlewares",
    "traefik.http.routers.<hole>.priority",
    "traefik.http.routers.<hole>.rule",
    "traefik.http.routers.<hole>.service",
    // HTTP router TLS (#301). The bare flag, the resolver that issues
    // the certificate, and the one `domains` entry this module writes.
    "traefik.http.routers.<hole>.tls",
    "traefik.http.routers.<hole>.tls.certresolver",
    "traefik.http.routers.<hole>.tls.domains[0].main",
    "traefik.http.routers.<hole>.tls.domains[0].sans",
    // The load-balancer target, written both service-wide by `port` and
    // per-router by `http_service`.
    "traefik.http.services.<hole>.loadbalancer.server.port",
    // The TCP mirror of all of it. Same keys, one segment over, which
    // is exactly why the list is worth keeping: `tcp` is the segment a
    // copy-paste forgets to change.
    "traefik.tcp.routers.<hole>.entrypoints",
    "traefik.tcp.routers.<hole>.middlewares",
    "traefik.tcp.routers.<hole>.priority",
    "traefik.tcp.routers.<hole>.rule",
    "traefik.tcp.routers.<hole>.service",
    "traefik.tcp.routers.<hole>.tls",
    "traefik.tcp.routers.<hole>.tls.certresolver",
    "traefik.tcp.routers.<hole>.tls.domains[0].main",
    "traefik.tcp.routers.<hole>.tls.domains[0].sans",
    // TCP only: hand the connection on still encrypted. An HTTP router
    // that doesn't decrypt has nothing to route on, so there is no HTTP
    // counterpart to miss.
    "traefik.tcp.routers.<hole>.tls.passthrough",
    "traefik.tcp.services.<hole>.loadbalancer.server.port",
];

/// One key with every `{{...}}` replaced by `<hole>`.
///
/// A hand-rolled scan rather than a regex: the crate pulls in no regex
/// dependency, and the grammar here is two fixed delimiters.
fn normalize(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut rest = key;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        out.push_str("<hole>");
        match rest[open + 2..].find("}}") {
            Some(close) => rest = &rest[open + 2 + close + 2..],
            // An unterminated `{{` is a diagnostic codegen raises, and
            // pinning the rest of the key is more useful here than
            // panicking about it.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Every label key the module's templates write, normalized and
/// deduplicated.
fn keys_the_module_writes() -> BTreeSet<String> {
    let program = hl_parser::parse(MODULE).expect("`std:traefik` has to parse");
    program
        .decls
        .iter()
        .filter_map(|decl| match decl {
            TopDecl::Template(template) => Some(&template.fields.labels.entries),
            _ => None,
        })
        .flatten()
        .map(|entry: &LabelEntry| normalize(entry.key.text()))
        .collect()
}

/// The inventory itself: what the module writes is what this file says
/// it writes, no more and no less.
#[test]
fn every_key_the_module_writes_is_a_documented_traefik_key() {
    let written = keys_the_module_writes();
    let documented: BTreeSet<String> = DOCUMENTED_KEYS.iter().map(|key| key.to_string()).collect();

    let undocumented: Vec<_> = written.difference(&documented).collect();
    let unwritten: Vec<_> = documented.difference(&written).collect();

    assert!(
        undocumented.is_empty() && unwritten.is_empty(),
        "`std:traefik`'s label keys no longer match this file's inventory.\n\
         \n\
         Written by the module but not listed here: {undocumented:#?}\n\
         Listed here but no longer written: {unwritten:#?}\n\
         \n\
         This is the check asking you to confirm the key against \
         Traefik's own docs rather than to re-bless a snapshot. If the \
         new key is right, add it to `DOCUMENTED_KEYS` with a comment \
         saying what Traefik does with it."
    );
}

/// A key the module can't write is a key no `DOCUMENTED_KEYS` entry
/// should hold. Guards the inventory itself against the one mistake it
/// invites: listing a key Traefik supports but this module doesn't, so
/// that a later typo lands on a line that's already there.
#[test]
fn the_inventory_holds_no_duplicates_and_stays_sorted() {
    let mut sorted = DOCUMENTED_KEYS.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        DOCUMENTED_KEYS.len(),
        "`DOCUMENTED_KEYS` holds a duplicate"
    );
    assert_eq!(
        sorted, DOCUMENTED_KEYS,
        "`DOCUMENTED_KEYS` is out of order — keep it sorted so a new key \
         lands beside the ones it's a variation of"
    );
}

/// The normalization the inventory reads through, pinned on its own so
/// a failure in the test above is never this function's fault.
#[test]
fn normalize_collapses_every_hole() {
    assert_eq!(normalize("traefik.enable"), "traefik.enable");
    assert_eq!(
        normalize("traefik.http.routers.{{router}}.rule"),
        "traefik.http.routers.<hole>.rule"
    );
    assert_eq!(
        normalize("traefik.http.services.{{name}}.loadbalancer.server.port"),
        "traefik.http.services.<hole>.loadbalancer.server.port"
    );
    assert_eq!(normalize("{{a}}.{{b}}"), "<hole>.<hole>");
    assert_eq!(normalize("trailing.{{unterminated"), "trailing.<hole>");
}
