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
