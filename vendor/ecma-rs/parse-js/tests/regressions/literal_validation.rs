use parse_js::error::SyntaxErrorType;
use parse_js::parse;

#[test]
fn duplicate_regex_flags_are_rejected() {
  let err = parse("/value/gg").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regex flags")
  );
}

#[test]
fn regex_unterminated_named_backreference_is_rejected() {
  let err = parse("/\\k</u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_empty_named_backreference_is_rejected() {
  let err = parse("/\\k<>/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_named_backreference_invalid_escape_in_name_is_rejected() {
  let err = parse("/\\k<\\u{}>/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_named_backreference_non_unicode_escape_in_name_is_rejected() {
  let err = parse("/\\k<\\x61>/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_invalid_unicode_escape_in_charset_is_rejected() {
  let err = parse("/[\\u{}]/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn invalid_string_escape_reports_error() {
  let err = parse("\"\\u{110000}\"").unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::InvalidCharacterEscape);
}

#[test]
fn string_with_line_terminator_is_invalid() {
  let err = parse("'line\nbreak'").unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::LineTerminatorInString);
}

#[test]
fn string_allows_unescaped_line_separator_codepoints() {
  let source = format!("'hello{}world'", '\u{2028}');
  parse(&source).unwrap();
  let source = format!("'hello{}world'", '\u{2029}');
  parse(&source).unwrap();
}

#[test]
fn string_allows_surrogate_escapes() {
  // Rust `String` cannot represent surrogate code points directly; the parser
  // maps them to U+FFFD while still accepting the literal.
  parse("\"\\u{D800}\"").unwrap();
  parse("\"\\uD800\"").unwrap();
  // Surrogate pair should decode into a valid scalar.
  parse("\"\\uD83D\\uDE00\"").unwrap();
}

#[test]
fn invalid_template_escape_reports_error() {
  let err = parse("`\\u{110000}`").unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::InvalidCharacterEscape);
}

#[test]
fn tagged_templates_allow_invalid_escapes() {
  assert!(parse("tag`\\u{110000}`").is_ok());
}

#[test]
fn regex_unicode_braced_escape_is_checked_in_unicode_mode() {
  let err = parse("/\\u{110000}/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_unicode_braced_escape_is_not_treated_as_unicode_escape_without_u_flag() {
  // In non-UnicodeMode, `\\u{...}` is an identity escape (`u`) followed by `{...}` which is parsed
  // as a quantifier/literal depending on context; it must not be rejected during parsing.
  assert!(parse("/\\u{41}/").is_ok());
}

#[test]
fn regex_unicode_braced_escape_is_checked_inside_character_classes() {
  let err = parse("/[\\u{110000}]/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_unicode_braced_escape_is_checked_inside_unicode_sets_character_classes() {
  let err = parse("/[\\u{110000}]/v").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_unicode_braced_escape_is_checked_inside_named_group_names() {
  let err = parse("/(?<a\\u{110000}>.)/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_x_escape_is_identity_in_non_unicode_mode() {
  assert!(parse("/\\x/").is_ok());
  let err = parse("/\\x/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn annex_b_incomplete_hex_and_unicode_regex_escapes_are_identity_in_non_unicode_mode() {
  // Per Annex B, incomplete `\x`/`\u` escapes fall through to IdentityEscape in non-unicode mode.
  assert!(parse("/\\xa/").is_ok());
  assert!(parse("/\\u/").is_ok());
  assert!(parse("/\\ua/").is_ok());
}

#[test]
fn incomplete_hex_and_unicode_regex_escapes_are_errors_in_unicode_mode() {
  // UnicodeMode (`/u` or `/v`) uses the stricter grammar where incomplete escapes are early errors.
  for src in ["/\\xa/u", "/\\u/u", "/\\ua/u"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}

#[test]
fn unicode_mode_invalid_regex_escapes_are_rejected() {
  for src in ["/\\a/u", "/[\\a]/u", "/\\c!/u", "/\\01/u", "/[\\1]/u"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }

  // Still accept valid unicode-mode identity escapes.
  assert!(parse("/\\^/u").is_ok());
  assert!(parse("/[\\-]/u").is_ok());
}
