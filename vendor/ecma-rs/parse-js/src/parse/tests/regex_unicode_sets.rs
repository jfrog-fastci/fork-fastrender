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
  assert!(parse_with_options("let r = /./uv;", opts).is_err());
  assert!(parse_with_options("let r = /./vu;", opts).is_err());
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
  for pat in ["[(]", "[)]", "[[]", "[{]", "[}]", "[/]", "[-]", "[|]", "[**]", "[&&]"] {
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
}

