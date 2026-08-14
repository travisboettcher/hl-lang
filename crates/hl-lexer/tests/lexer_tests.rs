use hl_lexer::{LexError, Lexer, TokenKind};

fn kinds(source: &str) -> Vec<TokenKind> {
    Lexer::tokenize(source)
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect()
}

fn single_token(source: &str) -> hl_lexer::Token<'_> {
    let mut lexer = Lexer::new(source);
    let tok = lexer.next_token().unwrap();
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Eof,
        "expected exactly one token"
    );
    tok
}

// --- empty / whitespace ---

#[test]
fn empty_input_yields_eof() {
    assert_eq!(kinds(""), vec![TokenKind::Eof]);
}

#[test]
fn whitespace_only_yields_eof() {
    assert_eq!(kinds("   \t\n\n  \t "), vec![TokenKind::Eof]);
}

// --- identifiers ---

#[test]
fn single_ident() {
    let tok = single_token("service");
    assert_eq!(tok.kind, TokenKind::Ident);
    assert_eq!(tok.lexeme, "service");
}

#[test]
fn ident_with_digits_and_dashes() {
    assert_eq!(single_token("unless-stopped").kind, TokenKind::Ident);
    assert_eq!(single_token("media-server2").kind, TokenKind::Ident);
    assert_eq!(single_token("media-server2").lexeme, "media-server2");
}

#[test]
fn ident_leading_underscore() {
    let tok = single_token("_foo");
    assert_eq!(tok.kind, TokenKind::Ident);
    assert_eq!(tok.lexeme, "_foo");
}

#[test]
fn dash_inside_ident_is_fine() {
    // Contrast case for the dangling-dash tests below: '-' mid-identifier
    // is just an ordinary continuation character, not an error.
    assert_eq!(kinds("foo-bar"), vec![TokenKind::Ident, TokenKind::Eof]);
    assert_eq!(
        kinds("unless-stopped"),
        vec![TokenKind::Ident, TokenKind::Eof]
    );
}

// --- numbers ---

#[test]
fn number_basic() {
    let tok = single_token("8096");
    assert_eq!(tok.kind, TokenKind::Number);
    assert_eq!(tok.lexeme, "8096");
}

#[test]
fn number_leading_zeros() {
    let tok = single_token("007");
    assert_eq!(tok.kind, TokenKind::Number);
    assert_eq!(tok.lexeme, "007");
}

#[test]
fn number_immediately_followed_by_dot_errors() {
    // No float support: '8096' is a complete Number, then '.' is its own
    // unexpected-character error on the next call.
    let mut lexer = Lexer::new("8096.5");
    let first = lexer.next_token().unwrap();
    assert_eq!(first.kind, TokenKind::Number);
    assert_eq!(first.lexeme, "8096");
    let err = lexer.next_token().unwrap_err();
    assert!(matches!(err, LexError::UnexpectedChar { ch: '.', .. }));
}

// --- strings ---

#[test]
fn string_basic() {
    let tok = single_token(r#""hello""#);
    assert_eq!(tok.kind, TokenKind::Str);
    assert_eq!(tok.lexeme, "hello");
}

#[test]
fn string_empty() {
    let tok = single_token(r#""""#);
    assert_eq!(tok.kind, TokenKind::Str);
    assert_eq!(tok.lexeme, "");
}

#[test]
fn string_contains_interpolation_markers_as_plain_text() {
    let tok = single_token(r#""{{name}}.internal.techdebtor.io""#);
    assert_eq!(tok.kind, TokenKind::Str);
    assert_eq!(tok.lexeme, "{{name}}.internal.techdebtor.io");
}

#[test]
fn unterminated_string_at_eof() {
    let mut lexer = Lexer::new(r#""abc"#);
    let err = lexer.next_token().unwrap_err();
    assert!(matches!(err, LexError::UnterminatedString { .. }));
}

#[test]
fn unterminated_string_at_newline() {
    let mut lexer = Lexer::new("\"abc\ndef\"");
    let err = lexer.next_token().unwrap_err();
    assert!(matches!(err, LexError::UnterminatedString { .. }));
}

#[test]
fn string_cannot_contain_literal_quote() {
    // No escapes: "a"b"" lexes as Str("a"), Ident("b"), then a second
    // opening quote that immediately hits EOF unterminated.
    let mut lexer = Lexer::new(r#""a"b""#);
    let a = lexer.next_token().unwrap();
    assert_eq!(a.kind, TokenKind::Str);
    assert_eq!(a.lexeme, "a");
    let b = lexer.next_token().unwrap();
    assert_eq!(b.kind, TokenKind::Ident);
    assert_eq!(b.lexeme, "b");
    let err = lexer.next_token().unwrap_err();
    assert!(matches!(err, LexError::UnterminatedString { .. }));
}

// --- punctuation ---

#[test]
fn all_punctuation_sequence() {
    use TokenKind::*;
    let expected = vec![
        LBrace, RBrace, LBracket, RBracket, LParen, RParen, Colon, Equals, Arrow, Comma, Eof,
    ];
    assert_eq!(kinds("{ } [ ] ( ) : = -> ,"), expected);
}

#[test]
fn arrow_is_one_token_not_two() {
    let tok = single_token("->");
    assert_eq!(tok.kind, TokenKind::Arrow);
    assert_eq!(tok.lexeme, "->");
}

#[test]
fn lone_dash_is_error() {
    let mut lexer = Lexer::new("-");
    assert!(matches!(
        lexer.next_token(),
        Err(LexError::DanglingDash { .. })
    ));
}

#[test]
fn dash_before_space_is_error() {
    let mut lexer = Lexer::new("- ");
    assert!(matches!(
        lexer.next_token(),
        Err(LexError::DanglingDash { .. })
    ));
}

#[test]
fn double_dash_is_error() {
    let mut lexer = Lexer::new("--");
    assert!(matches!(
        lexer.next_token(),
        Err(LexError::DanglingDash { .. })
    ));
}

#[test]
fn dash_before_number_is_error() {
    // No signed-number support in this grammar.
    let mut lexer = Lexer::new("-5");
    assert!(matches!(
        lexer.next_token(),
        Err(LexError::DanglingDash { .. })
    ));
}

// --- template keyword ---

#[test]
fn template_keyword_recognized() {
    let tok = single_token("template");
    assert_eq!(tok.kind, TokenKind::Template);
}

#[test]
fn template_prefix_is_still_ident() {
    assert_eq!(single_token("templates").kind, TokenKind::Ident);
    assert_eq!(single_token("template_x").kind, TokenKind::Ident);
}

// --- comments ---

#[test]
fn line_comment_skipped() {
    assert_eq!(
        kinds("# comment\nservice"),
        vec![TokenKind::Ident, TokenKind::Eof]
    );
}

#[test]
fn comment_at_eof_no_trailing_newline() {
    assert_eq!(kinds("# comment"), vec![TokenKind::Eof]);
}

#[test]
fn comment_only_file() {
    assert_eq!(kinds("# one\n# two\n\n# three"), vec![TokenKind::Eof]);
}

// --- line endings ---

#[test]
fn crlf_line_endings() {
    let mut lexer = Lexer::new("service\r\nnetwork\r\n");
    let first = lexer.next_token().unwrap();
    assert_eq!(first.kind, TokenKind::Ident);
    assert_eq!(first.lexeme, "service");
    assert_eq!(first.span.line, 1);

    let second = lexer.next_token().unwrap();
    assert_eq!(second.kind, TokenKind::Ident);
    assert_eq!(second.lexeme, "network");
    assert_eq!(
        second.span.line, 2,
        "a \\r\\n pair must increment the line counter exactly once"
    );
}

// --- unexpected characters ---

#[test]
fn unexpected_char_errors() {
    for ch in ['@', '%', '!', ';', '\\'] {
        let text = ch.to_string();
        let mut lexer = Lexer::new(&text);
        match lexer.next_token() {
            Err(LexError::UnexpectedChar { ch: got, .. }) => assert_eq!(got, ch),
            other => panic!("expected UnexpectedChar for {ch:?}, got {other:?}"),
        }
    }
}

#[test]
fn unexpected_char_dot() {
    let mut lexer = Lexer::new(".");
    assert!(matches!(
        lexer.next_token(),
        Err(LexError::UnexpectedChar { ch: '.', .. })
    ));
}

// --- spans ---

#[test]
fn span_byte_offsets_exact() {
    let mut lexer = Lexer::new("a:1");
    let a = lexer.next_token().unwrap();
    assert_eq!((a.span.start, a.span.end), (0, 1));
    let colon = lexer.next_token().unwrap();
    assert_eq!((colon.span.start, colon.span.end), (1, 2));
    let one = lexer.next_token().unwrap();
    assert_eq!((one.span.start, one.span.end), (2, 3));
}

#[test]
fn tight_packing_no_whitespace() {
    let mut lexer = Lexer::new("a:1");
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Ident);
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Colon);
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Number);
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Eof);
}

#[test]
fn line_col_tracking_multiline() {
    let source = "service jellyfin {\n  image \"x\"\n  restart unless-stopped\n}\n";
    let tokens = Lexer::tokenize(source).unwrap();
    // "restart" is the first token on line 3.
    let restart = tokens.iter().find(|t| t.lexeme == "restart").unwrap();
    assert_eq!(restart.span.line, 3);
    assert_eq!(restart.span.col, 3);
}

#[test]
fn eof_span_is_zero_width_at_end() {
    let tokens = Lexer::tokenize("a").unwrap();
    let eof = tokens.last().unwrap();
    assert_eq!(eof.kind, TokenKind::Eof);
    assert_eq!(eof.span.start, eof.span.end);
    assert_eq!(eof.span.start, "a".len());
}

#[test]
fn empty_input_eof_span_is_zero() {
    let tokens = Lexer::tokenize("").unwrap();
    let eof = tokens.last().unwrap();
    assert_eq!(eof.span.start, 0);
    assert_eq!(eof.span.end, 0);
}

#[test]
fn string_span_includes_quotes_but_lexeme_does_not() {
    let tok = single_token(r#""hello""#);
    assert_eq!(tok.lexeme.len(), 5); // "hello" without quotes
    assert_eq!(tok.span.end - tok.span.start, tok.lexeme.len() + 2);
}

// --- iterator fusing behavior ---

#[test]
fn iterator_yields_eof_once_then_none() {
    let mut lexer = Lexer::new("a");
    assert!(matches!(lexer.next(), Some(Ok(_)))); // Ident
    assert!(matches!(lexer.next(), Some(Ok(tok)) if tok.kind == TokenKind::Eof));
    assert!(lexer.next().is_none());
}

#[test]
fn iterator_fuses_after_error() {
    let mut lexer = Lexer::new("-a");
    assert!(matches!(
        lexer.next(),
        Some(Err(LexError::DanglingDash { .. }))
    ));
    assert!(lexer.next().is_none());
}
