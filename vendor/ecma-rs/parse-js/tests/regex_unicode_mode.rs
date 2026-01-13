use parse_js::error::SyntaxErrorType;
use parse_js::parse;

#[test]
fn unicode_mode_rejects_invalid_identity_escape() {
  let err = parse(r"/\M/u").unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("valid regular expression"));
}

#[test]
fn unicode_mode_rejects_invalid_class_control_escape() {
  let err = parse(r"/[\c0]/u").unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("valid regular expression"));
}

#[test]
fn unicode_mode_accepts_common_escapes() {
  parse(r"/\w\s\u{61}/u").unwrap();
  parse(r"/[\w\cA\-]/u").unwrap();
  parse(r"/\//u").unwrap();
}

