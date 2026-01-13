use crate::error::SyntaxErrorType;
use crate::parse_with_options;
use crate::Dialect;
use crate::ParseOptions;
use crate::SourceType;

fn ecma_script_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  }
}

#[test]
fn rejects_regex_literal_with_both_u_and_v_flags() {
  let opts = ecma_script_opts();
  let err = parse_with_options("/./uv;", opts).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("valid regex flags"));
  let err = parse_with_options("/./vu;", opts).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("valid regex flags"));
}

#[test]
fn accepts_unicode_property_escape_shape() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\p{ASCII_Hex_Digit}/u;", opts).is_ok());
  assert!(parse_with_options("let r = /\\p{Script=Han}/u;", opts).is_ok());
}

#[test]
fn rejects_unicode_property_of_strings_without_v() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\p{RGI_Emoji}/u;", opts).is_err());
}

#[test]
fn rejects_unicode_property_of_strings_in_p_negated_in_v() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\P{RGI_Emoji}/v;", opts).is_err());
  assert!(parse_with_options("let r = /[^\\p{RGI_Emoji}]/v;", opts).is_err());
}

#[test]
fn rejects_unicode_sets_breaking_change_patterns() {
  let opts = ecma_script_opts();
  for pat in [
    "[(]",
    "[)]",
    "[[]",
    "[{]",
    "[}]",
    "[/]",
    "[-]",
    "[|]",
    "[&&]",
    "[!!]",
    "[##]",
    "[$$]",
    "[%%]",
    "[**]",
    "[++]",
    "[,,]",
    "[..]",
    "[::]",
    "[;;]",
    "[<<]",
    "[==]",
    "[>>]",
    "[??]",
    "[@@]",
    "[``]",
    "[~~]",
    "[^^^]",
    "[_^^]",
  ] {
    let src = format!("let r = /{pat}/v;");
    assert!(parse_with_options(&src, opts).is_err(), "expected {src} to fail");
  }
}

#[test]
fn accepts_unicode_sets_examples() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /^[[0-9]_]+$/v;", opts).is_ok());
  assert!(parse_with_options("let r = /^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$/v;", opts).is_ok());
  assert!(parse_with_options("let r = /^[\\p{ASCII_Hex_Digit}_]+$/v;", opts).is_ok());
  assert!(parse_with_options("let r = /[]/v;", opts).is_ok());
}

#[test]
fn rejects_unicode_sets_and_operator_lookahead_early_errors() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /[(a)]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[a&&&b]/v;", opts).is_err());
}

#[test]
fn accepts_q_disjunction_in_negated_class_when_non_stringy() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /[^\\q{a|b}]/v;", opts).is_ok());
  assert!(parse_with_options("let r = /[^\\q{ab}]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[^\\q{}]/v;", opts).is_err());
}

#[test]
fn parses_test262_unicode_sets_generated_files() {
  // Smoke-test the vendored test262 `unicodeSets/generated` corpus: these should be parseable so
  // tests can reach runtime (even if RegExp v-mode execution is not implemented yet).
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/unicodeSets/generated");
  if !root.is_dir() {
    // Some distributions of `parse-js` may not vendor the test262 corpus.
    return;
  }

  let opts = ecma_script_opts();
  for entry in std::fs::read_dir(&root).expect("read unicodeSets/generated dir") {
    let entry = entry.expect("read_dir entry");
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("js") {
      continue;
    }
    let src = std::fs::read_to_string(&path).expect("read test file");
    if let Err(err) = parse_with_options(&src, opts) {
      panic!("failed to parse {}: {err}", path.display());
    }
  }
}
