use hl_lexer::{LexError, Lexer, TokenKind, unescape};

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
fn number_immediately_followed_by_dot_is_not_a_float() {
    // No float support: '8096' is a complete Number, '.' is its own Dot
    // token (used for qualified references, not decimals), and '5' is a
    // second, separate Number — never one float literal.
    assert_eq!(
        kinds("8096.5"),
        vec![
            TokenKind::Number,
            TokenKind::Dot,
            TokenKind::Number,
            TokenKind::Eof
        ]
    );
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
fn string_cannot_contain_an_unescaped_quote() {
    // An unescaped `"` always closes the literal, so "a"b"" lexes as
    // Str("a"), Ident("b"), then a second opening quote that immediately
    // hits EOF unterminated.
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

// --- string escapes (#181) ---

/// The lexeme is source text, so it still spells each escape the way it
/// was written; `unescape` is what turns it into the character it stands
/// for.
#[test]
fn every_escape_decodes_to_the_character_it_stands_for() {
    for (source, decoded) in [
        (r#""a\"b""#, "a\"b"),
        (r#""a\\b""#, "a\\b"),
        (r#""a\nb""#, "a\nb"),
        (r#""a\tb""#, "a\tb"),
        (r#""a\rb""#, "a\rb"),
    ] {
        let tok = single_token(source);
        assert_eq!(tok.kind, TokenKind::Str, "{source}");
        assert_eq!(tok.lexeme, &source[1..source.len() - 1], "{source}");
        assert_eq!(unescape(tok.lexeme, tok.span).unwrap(), decoded, "{source}");
    }
}

/// The whole point of `\"`: the escaped quote is content, so the literal
/// runs past it to the next unescaped one.
#[test]
fn escaped_quote_does_not_close_the_string() {
    let tok = single_token(r#""say \"hi\" now""#);
    assert_eq!(tok.kind, TokenKind::Str);
    assert_eq!(unescape(tok.lexeme, tok.span).unwrap(), r#"say "hi" now"#);
}

/// An escaped backslash is one character, and doesn't escape whatever
/// follows it — `"C:\\"` is a trailing lone backslash, not an escaped
/// quote running past the end of the literal.
#[test]
fn escaped_backslash_does_not_escape_the_next_character() {
    let tok = single_token(r#""C:\\""#);
    assert_eq!(unescape(tok.lexeme, tok.span).unwrap(), r"C:\");
}

/// A sequence outside the supported set is a lex error rather than
/// content that quietly means something other than what it says (#181,
/// the same silently-wrong-output failure #168 was about).
#[test]
fn unknown_escape_is_an_error() {
    let mut lexer = Lexer::new(r#""a\qb""#);
    let err = lexer.next_token().unwrap_err();
    assert!(
        matches!(err, LexError::UnknownEscape { ch: 'q', .. }),
        "{err:?}"
    );
}

/// The error points at the offending backslash, not at the literal it
/// sits in — and its offsets stay measured in source bytes while its
/// column stays measured in characters, which only a multi-byte
/// character ahead of the escape can tell apart.
#[test]
fn unknown_escape_span_points_at_the_backslash() {
    let source = "\n  \"caf\u{e9}\\qx\"";
    let mut lexer = Lexer::new(source);
    let err = lexer.next_token().unwrap_err();
    let span = err.span();
    assert_eq!(span.line, 2);
    assert_eq!(span.col, 8);
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        r"\q",
        "span {span:?} of {source:?}"
    );
}

/// A `\` before the closing quote escapes that quote, so the literal
/// runs on and hits the end of input unterminated. It is never silently
/// dropped or passed through.
#[test]
fn backslash_before_the_closing_quote_leaves_the_string_unterminated() {
    let mut lexer = Lexer::new(r#"image "C:\""#);
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Ident);
    let err = lexer.next_token().unwrap_err();
    assert!(
        matches!(err, LexError::UnterminatedString { .. }),
        "{err:?}"
    );
}

/// There is no line-continuation escape: a `\` at the end of a line
/// escapes nothing, and the literal ends there, unterminated.
///
/// The span runs *through* the trailing backslash, because the literal
/// consumed it. Asserting that rather than just the error variant is
/// what pins the offset the unterminated arm reports: the backslash is
/// the last thing the scan takes, and nothing after it ever overwrites
/// the running end offset the way an escaped character would.
#[test]
fn backslash_before_a_newline_does_not_continue_the_line() {
    let source = "\"abc\\\ndef\"";
    let mut lexer = Lexer::new(source);
    let err = lexer.next_token().unwrap_err();
    assert!(
        matches!(err, LexError::UnterminatedString { .. }),
        "{err:?}"
    );
    let span = err.span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "\"abc\\",
        "span {span:?} of {source:?}"
    );
}

/// The same at end of input, where there is no newline to stop at
/// either. A trailing `\` escapes nothing and there is no character
/// after it to join the content, so the literal ends unterminated with
/// the backslash inside its span.
#[test]
fn backslash_at_end_of_input_leaves_the_string_unterminated() {
    let source = r#""abc\"#;
    let mut lexer = Lexer::new(source);
    let err = lexer.next_token().unwrap_err();
    assert!(
        matches!(err, LexError::UnterminatedString { .. }),
        "{err:?}"
    );
    let span = err.span();
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        r#""abc\"#,
        "span {span:?} of {source:?}"
    );
}

/// Escapes don't disturb the span/lexeme relationship the rest of the
/// pipeline relies on: the lexeme is still exactly the source bytes
/// between the quotes, even though the decoded value is shorter.
#[test]
fn escaped_string_span_still_covers_its_source_text() {
    let tok = single_token(r#""a\nb""#);
    assert_eq!(tok.lexeme, r"a\nb");
    assert_eq!(
        (tok.span.end - tok.span.start) as usize,
        tok.lexeme.len() + 2
    );
    assert_eq!(unescape(tok.lexeme, tok.span).unwrap().len(), 3);
}

/// Every lex error in a file is reported in one pass (#87), and an
/// invalid escape is no exception.
#[test]
fn tokenize_collecting_errors_finds_every_bad_escape() {
    let errors = Lexer::tokenize_collecting_errors("\"a\\q\"\n\"b\\z\"").unwrap_err();
    assert_eq!(errors.len(), 2);
    assert!(matches!(errors[0], LexError::UnknownEscape { ch: 'q', .. }));
    assert!(matches!(errors[1], LexError::UnknownEscape { ch: 'z', .. }));
}

// --- punctuation ---

#[test]
fn all_punctuation_sequence() {
    use TokenKind::*;
    let expected = vec![
        LBrace, RBrace, LBracket, RBracket, LParen, RParen, Colon, Equals, Arrow, Comma, Dot,
        Dollar, Eof,
    ];
    assert_eq!(kinds("{ } [ ] ( ) : = -> , . $"), expected);
}

#[test]
fn dot_token() {
    let tok = single_token(".");
    assert_eq!(tok.kind, TokenKind::Dot);
    assert_eq!(tok.lexeme, ".");
}

#[test]
fn dollar_token() {
    let tok = single_token("$");
    assert_eq!(tok.kind, TokenKind::Dollar);
    assert_eq!(tok.lexeme, "$");
}

#[test]
fn dollar_param_reference_tokenizes_as_dollar_ident() {
    assert_eq!(
        kinds("$port"),
        vec![TokenKind::Dollar, TokenKind::Ident, TokenKind::Eof]
    );
}

#[test]
fn qualified_reference_tokenizes_as_ident_dot_ident() {
    assert_eq!(
        kinds("traefik.traefik-net"),
        vec![
            TokenKind::Ident,
            TokenKind::Dot,
            TokenKind::Ident,
            TokenKind::Eof
        ]
    );
}

#[test]
fn dot_does_not_interact_with_number_scanning() {
    // NUMBER is integer-only (no decimal point) — a `.` next to digits
    // is just an ordinary Dot token, not part of the number.
    assert_eq!(
        kinds("1.5"),
        vec![
            TokenKind::Number,
            TokenKind::Dot,
            TokenKind::Number,
            TokenKind::Eof
        ]
    );
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

// --- batched errors (#87) ---

#[test]
fn tokenize_collecting_errors_returns_ok_with_no_errors() {
    let tokens = Lexer::tokenize_collecting_errors("service s {\n  image \"x\"\n}\n")
        .expect("valid source should collect zero errors");
    assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
}

#[test]
fn tokenize_collecting_errors_finds_every_unexpected_char_in_one_pass() {
    // Two independent unexpected characters, on different lines — the
    // single-error `tokenize`/iterator path would only ever report the
    // first and stop; this collects both.
    let errors = Lexer::tokenize_collecting_errors("a @ b\nc % d\n")
        .expect_err("expected both unexpected characters to be reported");
    assert_eq!(errors.len(), 2);
    assert!(matches!(
        errors[0],
        LexError::UnexpectedChar { ch: '@', .. }
    ));
    assert!(matches!(
        errors[1],
        LexError::UnexpectedChar { ch: '%', .. }
    ));
    assert_eq!(errors[0].span().line, 1);
    assert_eq!(errors[1].span().line, 2);
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
    assert_eq!(eof.span.start as usize, "a".len());
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
    assert_eq!(
        (tok.span.end - tok.span.start) as usize,
        tok.lexeme.len() + 2
    );
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
