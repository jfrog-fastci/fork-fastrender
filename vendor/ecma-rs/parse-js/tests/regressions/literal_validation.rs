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
fn unicode_mode_rejects_right_bracket_as_pattern_character() {
  // In UnicodeMode (`u`/`v`), `]` is not a PatternCharacter (Annex B extension disabled).
  let err = parse("/]/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
  let err = parse("/]/v").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );

  // Non-unicode mode still permits `]` as a literal PatternCharacter.
  parse("/]/").unwrap();
}

#[test]
fn empty_character_classes_remain_valid() {
  // The ECMAScript RegExp grammar permits empty character classes (`[]` / `[^]`),
  // which are used in the wild (e.g. `[^]` as a "match-any" hack).
  for src in ["/[]/", "/[]/u", "/[]/v", "/[^]/", "/[^]/u", "/[^]/v"] {
    parse(src).unwrap();
  }

  // Still applies the UnicodeMode `]` restriction after a character class has ended.
  parse("/[]]/").unwrap();
  for src in ["/[]]/u", "/[]]/v"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}

#[test]
fn unicode_mode_rejects_character_class_escape_ranges() {
  for src in [
    r"/[\d-a]/u",
    r"/[a-\d]/u",
    r"/[\w-\w]/u",
    r"/[\w-\uFFFF]/u",
    r"/[\W-\uFFFF]/u",
    r"/[\d-\uFFFF]/u",
    r"/[\D-\uFFFF]/u",
    r"/[\s-\uFFFF]/u",
    r"/[\S-\uFFFF]/u",
    r"/[\uFFFF-\w]/u",
    r"/[\uFFFF-\W]/u",
    r"/[\uFFFF-\d]/u",
    r"/[\uFFFF-\D]/u",
    r"/[\uFFFF-\s]/u",
    r"/[\uFFFF-\S]/u",
    r"/[\W-\W]/u",
    r"/[\d-\d]/u",
    r"/[\D-\D]/u",
    r"/[\s-\s]/u",
    r"/[\S-\S]/u",
    r"/[\d-a]/v",
    r"/[a-\d]/v",
    r"/[\w-\w]/v",
    r"/[\w-\uFFFF]/v",
    r"/[\W-\uFFFF]/v",
    r"/[\d-\uFFFF]/v",
    r"/[\D-\uFFFF]/v",
    r"/[\s-\uFFFF]/v",
    r"/[\S-\uFFFF]/v",
    r"/[\uFFFF-\w]/v",
    r"/[\uFFFF-\W]/v",
    r"/[\uFFFF-\d]/v",
    r"/[\uFFFF-\D]/v",
    r"/[\uFFFF-\s]/v",
    r"/[\uFFFF-\S]/v",
    r"/[\W-\W]/v",
    r"/[\d-\d]/v",
    r"/[\D-\D]/v",
    r"/[\s-\s]/v",
    r"/[\S-\S]/v",
  ] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "expected parse-time syntax error for {src}"
    );
  }

  // These patterns are accepted in non-unicode mode (Annex B compatibility).
  parse(r"/[\d-a]/").unwrap();
  parse(r"/[a-\d]/").unwrap();
}

#[test]
fn regex_literal_rejects_out_of_order_character_class_ranges() {
  let err = parse(r"/[d-G]/").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn unicode_sets_mode_rejects_out_of_order_character_class_ranges() {
  let err = parse(r"/[d-G]/v").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );

  // Still accepts ranges where the endpoints are in ascending order.
  parse(r"/[G-d]/v").unwrap();
}

#[test]
fn unicode_mode_accepts_surrogate_pair_character_class_ranges() {
  // In UnicodeMode, surrogate pairs expressed via consecutive `\uXXXX` escapes form a single
  // character and can be used as range endpoints.
  parse(r"/[\uD83D\uDE00-\uD83D\uDE01]/u").unwrap();
  parse(r"/[\uD83D\uDE00-\uD83D\uDE01]/v").unwrap();

  // Non-UnicodeMode still treats them as separate code units and rejects the range (out of order).
  assert!(parse(r"/[\uD83D\uDE00-\uD83D\uDE01]/").is_err());
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
fn regex_named_backreference_requires_existing_group_in_unicode_mode() {
  let err = parse("/\\k<a>/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_named_backreference_requires_existing_group_in_non_unicode_mode() {
  let err = parse("/(?<a>a)\\k<b>/").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn regex_named_backreference_can_appear_before_group_definition() {
  parse("/\\k<a>(?<a>a)/u").unwrap();
}

#[test]
fn regex_duplicate_named_captures_are_rejected() {
  for src in ["/(?<a>a)(?<a>b)/", "/(?<a>a)(?<a>b)/u", "/(?<a>a)(?<a>b)/v"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}

#[test]
fn regex_duplicate_named_captures_are_allowed_in_disjoint_alternatives() {
  // Duplicate named capture groups are permitted if they occur in disjoint alternation branches
  // (see `regexp-duplicate-named-groups`).
  for src in ["/(?<a>a)|(?<a>b)/", "/(?<a>a)|(?<a>b)/u", "/(?<a>a)|(?<a>b)/v"] {
    parse(src).unwrap();
  }
}

#[test]
fn regex_duplicate_named_captures_in_disjoint_alternatives_are_allowed() {
  for src in [
    "/(?:(?<a>a)|(?<a>b))/",
    "/(?:(?<a>a)|(?<a>b))/u",
    "/(?:(?<a>a)|(?<a>b))/v",
    "/(?:(?<a>a)|(?<a>b))*/",
    "/(?:(?<a>a)|(?<a>b))*/u",
    "/(?:(?<a>a)|(?<a>b))*/v",
  ] {
    parse(src).unwrap();
  }
}

#[test]
fn regex_lookbehind_assertions_are_not_quantifiable() {
  for src in [
    "/(?<=a)+/",
    "/(?<!a)+/",
    "/(?<=a){2}/",
    "/(?<!a){2}/",
    "/(?<=a)*/",
    "/(?<!a)?/",
  ] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }

  // Plain lookbehind assertions are still valid patterns.
  parse("/(?<=a)b/").unwrap();
  parse("/(?<!a)b/").unwrap();
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
fn regex_unicode_braced_escape_is_checked_inside_unicode_sets_class_string_disjunctions() {
  let err = parse("/[\\q{\\u{110000}}]/v").unwrap_err();
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
  for src in [
    "/\\xa/u",
    "/\\u/u",
    "/\\ua/u",
    "/[\\xa]/u",
    "/[\\u]/u",
    "/[\\ua]/u",
    "/\\xa/v",
    "/\\u/v",
    "/\\ua/v",
    "/[\\xa]/v",
    "/[\\u]/v",
    "/[\\ua]/v",
  ] {
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
  for src in [
    "/\\a/u",
    "/[\\a]/u",
    "/\\c!/u",
    "/\\01/u",
    "/[\\1]/u",
    "/\\a/v",
    "/[\\a]/v",
    "/\\c!/v",
    "/\\01/v",
    "/[\\1]/v",
  ] {
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
  assert!(parse("/\\^/v").is_ok());
  assert!(parse("/[\\-]/v").is_ok());
}

#[test]
fn unicode_mode_escape_edge_cases() {
  // In non-unicode mode, Annex B permits treating invalid/incomplete `\c` escapes as literal
  // pattern characters.
  for src in ["/\\c/", "/\\c!/", "/[\\c]/", "/[\\c!]/"] {
    assert!(parse(src).is_ok(), "{src}");
  }
  // Invalid `\c` escapes inside character classes are treated as literal pattern characters and
  // must still participate in range validation.
  let err = parse("/[a-\\c]/").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );

  // In unicode mode (`u`/`v`), `\c` must be followed by an ASCII letter (including in character
  // classes).
  for src in ["/\\c/u", "/\\c/v", "/[\\c]/u", "/[\\c]/v"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }

  // `\\-` is only a valid identity escape inside character classes; outside it is a SyntaxError in
  // unicode mode.
  for src in ["/\\-/u", "/\\-/v"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
  assert!(parse("/-/u").is_ok());
  assert!(parse("/-/v").is_ok());

  // Decimal escapes must not reference more groups than exist.
  for src in ["/\\1/u", "/\\1/v", "/(a)\\2/u", "/(a)\\2/v"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
  assert!(parse("/(a)\\1/u").is_ok());
  assert!(parse("/(a)\\1/v").is_ok());
}

#[test]
fn unicode_mode_character_class_ranges_reject_property_escape_endpoints() {
  for src in [
    "/[\\p{Hex}--]/u",
    "/[--\\p{Hex}]/u",
    "/[\\p{Hex}-\\uFFFF]/u",
    "/[\\uFFFF-\\p{Hex}]/u",
  ] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}

#[test]
fn unicode_mode_character_class_ranges_reject_builtin_class_escape_endpoints() {
  for src in ["/[\\d-a]/u", "/[a-\\d]/u", "/[\\s-z]/u", "/[z-\\w]/u"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}
