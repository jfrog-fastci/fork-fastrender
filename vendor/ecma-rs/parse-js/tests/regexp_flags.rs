use parse_js::error::SyntaxErrorType;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn regexp_literal_with_uv_flags_is_early_error() {
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let err = parse_with_options("/./uv;", opts).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("valid regex flags"));
}

