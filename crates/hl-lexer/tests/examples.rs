//! Integration tests: lex the design doc's worked examples end-to-end and
//! assert the exact token-kind sequence produced.

use hl_lexer::{Lexer, TokenKind};

const JELLYFIN: &str = include_str!("fixtures/jellyfin.dsl");
const SYNCTHING: &str = include_str!("fixtures/syncthing.dsl");
const COMBINED: &str = include_str!("fixtures/combined.dsl");

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
        Template, Ident, LParen, Ident, RParen, LBrace, // networks [traefik-net]
        Ident, LBracket, Ident, RBracket, // restart unless-stopped
        Ident, Ident, // expose port as "{{name}}.internal.techdebtor.io"
        Ident, Ident, Ident, Str, // middleware local-ipwhitelist
        Ident, Ident, RBrace,
        // template authenticated { middleware forwardAuth-authentik }
        Template, Ident, LBrace, Ident, Ident, RBrace,
        // template linuxserver_app(puid, pgid) {
        Template, Ident, LParen, Ident, Comma, Ident, RParen, LBrace, // env PUID = puid
        Ident, Ident, Equals, Ident, // env PGID = pgid
        Ident, Ident, Equals, Ident, RBrace, // service syncthing {
        Ident, Ident, LBrace,
        // with internal_web { port: 8384 }, authenticated, linuxserver_app { puid: 1000, pgid: 100 }
        Ident, Ident, LBrace, Ident, Colon, Number, RBrace, Comma, Ident, Comma, Ident, LBrace,
        Ident, Colon, Number, Comma, Ident, Colon, Number, RBrace, // image "..."
        Ident, Str, // volume "syncthing-config" -> "/config"
        Ident, Str, Arrow, Str, RBrace, Eof,
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
fn syncthing_example_never_emits_template_as_plain_ident() {
    // Sanity check on the fixture itself: it declares three `template`
    // blocks, so exactly three Template tokens should appear, and the
    // string "template" should never leak through as an Ident lexeme.
    let tokens = Lexer::tokenize(SYNCTHING).unwrap();
    let template_count = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Template)
        .count();
    assert_eq!(template_count, 3);
    assert!(
        tokens
            .iter()
            .all(|t| !(t.kind == TokenKind::Ident && t.lexeme == "template"))
    );
}
