//! Integration tests: lex the design doc's worked examples end-to-end and
//! assert the exact token-kind sequence produced.

use hl_lexer::{Lexer, TokenKind, unescape};

const JELLYFIN: &str = include_str!("fixtures/jellyfin.hll");
const SYNCTHING: &str = include_str!("fixtures/syncthing.hll");
const COMBINED: &str = include_str!("fixtures/combined.hll");
const ESCAPES: &str = include_str!("fixtures/escapes.hll");

fn kinds(source: &str) -> Vec<TokenKind> {
    Lexer::tokenize(source)
        .unwrap_or_else(|err| panic!("unexpected lex error: {err}"))
        .into_iter()
        .map(|tok| tok.kind)
        .collect()
}

#[test]
fn jellyfin_example_lexes_to_expected_token_sequence() {
    use TokenKind::*;
    let expected = vec![
        Ident, Ident, LBrace, // service jellyfin {
        Ident, Str, // image "..."
        Ident, Number, Ident, Str, // expose 8096 as "..."
        Ident, Str, Arrow, Str, // volume "..." -> "..."
        Ident, Ident, Equals, Str, // env PUID = "..."
        Ident, Ident, // restart unless-stopped
        RBrace, Eof,
    ];
    assert_eq!(kinds(JELLYFIN), expected);
}

#[test]
fn syncthing_example_lexes_to_expected_token_sequence() {
    use TokenKind::*;
    let expected = vec![
        // template internal_web(port) {
        Ident, Ident, LParen, Ident, RParen, LBrace, // networks [traefik-net]
        Ident, LBracket, Ident, RBracket, // restart unless-stopped
        Ident, Ident, // expose $port as "{{name}}.internal.techdebtor.io"
        Ident, Dollar, Ident, Ident, Str, // router { middleware: local-ipwhitelist }
        Ident, LBrace, Ident, Colon, Ident, RBrace, RBrace,
        // template authenticated { router { middleware: forwardAuth-authentik } }
        Ident, Ident, LBrace, Ident, LBrace, Ident, Colon, Ident, RBrace, RBrace,
        // template linuxserver_app(puid, pgid) {
        Ident, Ident, LParen, Ident, Comma, Ident, RParen, LBrace, // env PUID = $puid
        Ident, Ident, Equals, Dollar, Ident, // env PGID = $pgid
        Ident, Ident, Equals, Dollar, Ident, RBrace, // service syncthing {
        Ident, Ident, LBrace,
        // with internal_web { port: 8384 }, authenticated, linuxserver_app { puid: 1000, pgid: 100 }
        Ident, Ident, LBrace, Ident, Colon, Number, RBrace, Comma, Ident, Comma, Ident, LBrace,
        Ident, Colon, Number, Comma, Ident, Colon, Number, RBrace, // image "..."
        Ident, Str, // volume syncthing-config -> "/config"
        Ident, Ident, Arrow, Str, RBrace, Eof,
    ];
    assert_eq!(kinds(SYNCTHING), expected);
}

#[test]
fn combined_fixture_lexes_without_error() {
    let tokens = Lexer::tokenize(COMBINED).expect("combined fixture should lex cleanly");
    assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);

    // The combined file is the two examples concatenated (plus extra
    // comments/blank lines, which contribute no tokens), so its non-Eof
    // token count should equal the sum of the two individual examples'
    // non-Eof token counts.
    let jellyfin_count = kinds(JELLYFIN).len() - 1;
    let syncthing_count = kinds(SYNCTHING).len() - 1;
    assert_eq!(tokens.len() - 1, jellyfin_count + syncthing_count);
}

#[test]
fn syncthing_example_lexes_template_as_a_plain_ident() {
    // Sanity check on the fixture itself: it declares three `template`
    // blocks, and since #258 removed the language's last reserved word,
    // each of those leads with an ordinary `Ident` whose lexeme happens
    // to be "template". The parser tells them apart by position — see
    // `hl_parser::Parser::parse_top_decl` — and the lexer no longer has
    // a variant that could.
    let tokens = Lexer::tokenize(SYNCTHING).unwrap();
    let template_count = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Ident && t.lexeme == "template")
        .count();
    assert_eq!(template_count, 3);
}

/// #181: the escapes fixture lexes cleanly, and each of its string
/// literals decodes to the value it spells — including the multi-line
/// one, which had no representation at all before.
///
/// The fixture doubles as a fuzz corpus seed: CI copies this directory
/// into `fuzz/corpus/` before every smoke run (see
/// `.github/workflows/ci.yml`), so the escape scanning added here starts
/// from an input that already exercises it.
#[test]
fn escapes_fixture_lexes_and_decodes() {
    let tokens = Lexer::tokenize(ESCAPES).expect("escapes fixture should lex cleanly");
    let strings: Vec<String> = tokens
        .iter()
        .filter(|tok| tok.kind == TokenKind::Str)
        .map(|tok| unescape(tok.lexeme, tok.span).unwrap().into_owned())
        .collect();
    assert_eq!(
        strings,
        vec![
            "nginx:latest",
            "sh -c \"exec nginx -g 'daemon off;'\"",
            "{\"log\": \"debug\"}",
            "first\nsecond\tthird\r",
            "C:\\logs",
        ]
    );
}
