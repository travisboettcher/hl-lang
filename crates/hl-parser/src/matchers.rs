//! Traefik's rule matchers, as one table (#228).
//!
//! A `router`'s [`rule`](crate::ast::Router::rule) is a boolean
//! expression over these — `Host("a") && !PathPrefix("/b")` — and two
//! passes need to agree about them. The parser checks that a written
//! matcher exists and has the right number of arguments; codegen checks
//! that it's legal in the namespace the router's `protocol` picked. Both
//! read this table, so there is one place a new matcher gets added and no
//! way for the two answers to drift apart.
//!
//! The names are Traefik's own, spelling included: a rule is something a
//! user copies out of a Traefik label or the Traefik documentation, and
//! renaming the matchers to match `hll`'s own snake_case field names
//! would put a translation step in the middle of that. `hll`'s
//! contribution is the string literals — Traefik delimits a matcher
//! argument with a backtick, which has no escape, so `hllc` writes those
//! and the user writes `"..."`.
//!
//! Arities are exact rather than minimums. Traefik's own reference states
//! one signature per matcher (`Header(key, value)`, `Host(domain)`, ...),
//! so a call with the wrong count is a mistake `hllc` can name at the
//! spot it was written rather than a rule Traefik rejects at load time.

/// One row of the matcher table.
pub struct Matcher {
    /// Traefik's own spelling, matched case-sensitively.
    pub name: &'static str,
    /// Exactly how many arguments the matcher takes.
    pub arity: usize,
    /// Legal on an HTTP router (`traefik.http.routers.*`).
    pub http: bool,
    /// Legal on a TCP router (`traefik.tcp.routers.*`).
    pub tcp: bool,
}

/// Every matcher `hllc` accepts, sorted by name so the diagnostic that
/// lists them reads in a predictable order.
///
/// The two namespaces barely overlap, and that's Traefik's design rather
/// than an accident worth smoothing over: a TCP router matches on the TLS
/// handshake, where there is no request line or header to read, so `Path`
/// and `Header` have nothing to look at and `HostSNI` exists precisely
/// because `Host` can't work there. `ClientIP` is the one matcher both
/// namespaces have, since a connection has a peer address either way.
pub static MATCHERS: &[Matcher] = &[
    Matcher {
        name: "ALPN",
        arity: 1,
        http: false,
        tcp: true,
    },
    Matcher {
        name: "ClientIP",
        arity: 1,
        http: true,
        tcp: true,
    },
    Matcher {
        name: "Header",
        arity: 2,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "HeaderRegexp",
        arity: 2,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "Host",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "HostRegexp",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "HostSNI",
        arity: 1,
        http: false,
        tcp: true,
    },
    Matcher {
        name: "HostSNIRegexp",
        arity: 1,
        http: false,
        tcp: true,
    },
    Matcher {
        name: "Method",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "Path",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "PathPrefix",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "PathRegexp",
        arity: 1,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "Query",
        arity: 2,
        http: true,
        tcp: false,
    },
    Matcher {
        name: "QueryRegexp",
        arity: 2,
        http: true,
        tcp: false,
    },
];

/// Looks a matcher up by its exact name.
///
/// Case-sensitive, on the same reasoning that picked Traefik's spellings
/// in the first place: accepting `host(...)` as an alias for `Host(...)`
/// would put two spellings of one matcher in the language, and a rule
/// that reads differently from the label it becomes is the thing this
/// field exists to avoid.
pub fn lookup(name: &str) -> Option<&'static Matcher> {
    MATCHERS.iter().find(|m| m.name == name)
}

/// Every matcher name, backtick-quoted and comma-joined, for the
/// `UnknownMatcher` diagnostic.
///
/// The whole set rather than a spelling guess: this crate has no
/// edit-distance helper and deliberately offers deterministic hints
/// instead (see [`crate::ParseError::UnknownField`], which names `raw`
/// rather than guessing at what was meant). Fourteen names is short
/// enough to print, and printing them answers "what *can* I write here?"
/// as well as "what did I typo?".
pub fn known_names() -> String {
    MATCHERS
        .iter()
        .map(|m| format!("`{}`", m.name))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_a_matcher_by_its_exact_name() {
        let m = lookup("PathPrefix").expect("PathPrefix is a matcher");
        assert_eq!(m.arity, 1);
        assert!(m.http);
        assert!(!m.tcp);
    }

    #[test]
    fn lookup_is_case_sensitive() {
        assert!(lookup("pathprefix").is_none());
        assert!(lookup("PATHPREFIX").is_none());
        assert!(lookup("Pathprefix").is_none());
    }

    #[test]
    fn client_ip_is_the_one_matcher_both_namespaces_share() {
        let both: Vec<_> = MATCHERS
            .iter()
            .filter(|m| m.http && m.tcp)
            .map(|m| m.name)
            .collect();
        assert_eq!(both, vec!["ClientIP"]);
    }

    /// Every row belongs to at least one namespace — a matcher legal
    /// nowhere would be a row nothing could ever reach.
    #[test]
    fn every_matcher_is_legal_somewhere() {
        for m in MATCHERS {
            assert!(m.http || m.tcp, "{} is legal in neither namespace", m.name);
        }
    }

    #[test]
    fn the_table_is_sorted_and_has_no_duplicates() {
        let names: Vec<_> = MATCHERS.iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn known_names_lists_every_matcher_backtick_quoted() {
        let listed = known_names();
        for m in MATCHERS {
            assert!(listed.contains(&format!("`{}`", m.name)), "{listed}");
        }
    }
}
