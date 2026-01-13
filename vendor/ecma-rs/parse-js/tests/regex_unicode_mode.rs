use parse_js::error::SyntaxErrorType;
use parse_js::parse;

#[test]
fn unicode_mode_rejects_invalid_identity_escape() {
  let err = parse(r"/\M/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn unicode_mode_rejects_invalid_class_control_escape() {
  let err = parse(r"/[\c0]/u").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn non_unicode_mode_allows_annex_b_class_control_escape() {
  parse(r"/\c0/").unwrap();
  parse(r"/[\c0]/").unwrap();
  parse(r"/\c_/").unwrap();
  parse(r"/[\c_]/").unwrap();
}

#[test]
fn unicode_mode_accepts_common_escapes() {
  parse(r"/\w\s\u{61}/u").unwrap();
  parse(r"/[\w\cA\-]/u").unwrap();
  parse(r"/\//u").unwrap();
}

#[test]
fn unicode_sets_mode_rejects_invalid_identity_escape_in_class_string_disjunction() {
  let err = parse(r"/[\q{\M}]/v").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn unicode_sets_mode_rejects_invalid_control_escape_in_class_string_disjunction() {
  let err = parse(r"/[\q{\c0}]/v").unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regular expression")
  );
}

#[test]
fn unicode_sets_mode_accepts_class_string_disjunction_escapes() {
  parse(r"/[\q{\cA\-\&\u{61}\}}]/v").unwrap();
}

#[test]
fn unicode_sets_mode_accepts_class_set_reserved_punctuator_escapes() {
  // In UnicodeSets mode (`/v`), certain punctuators are reserved because they can form operators
  // like `&&`/`--` or other reserved double-punctuator tokens. They can still be represented
  // literally via `\` escapes.
  parse(r"/[\&]/v").unwrap();
  parse(r"/[\&\&]/v").unwrap();
  parse(r"/[\!\!]/v").unwrap();
}

#[test]
fn unicode_mode_rejects_class_set_reserved_punctuator_escapes_without_unicode_sets() {
  // In Unicode mode without UnicodeSets (`/u`), these escapes are not valid identity escapes and
  // must be rejected.
  for src in [r"/[\&]/u", r"/[\&\&]/u", r"/[\!\!]/u"] {
    let err = parse(src).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "{src}"
    );
  }
}
