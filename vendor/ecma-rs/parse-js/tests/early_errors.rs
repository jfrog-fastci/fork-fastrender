use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

fn parse_ecma_script(source: &str) -> Result<parse_js::ast::node::Node<parse_js::ast::stx::TopLevel>, parse_js::error::SyntaxError> {
  parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    },
  )
}

#[test]
fn reserved_word_void_is_not_a_binding_identifier() {
  assert!(parse_ecma_script("var void = 1;").is_err());
  assert!(parse_ecma_script("var \\u{76}\\u{6f}\\u{69}\\u{64} = 1;").is_err());
}

#[test]
fn nullish_coalescing_cannot_mix_with_logical_operators() {
  assert!(parse_ecma_script("0 && 0 ?? true;").is_err());
  assert!(parse_ecma_script("0 ?? 0 || true;").is_err());
}

#[test]
fn optional_chaining_cannot_be_used_as_a_tagged_template() {
  assert!(parse_ecma_script("a?.fn`hello`;").is_err());
  assert!(parse_ecma_script("a?.fn\n`hello`;").is_err());
}

#[test]
fn object_literals_cannot_have_private_names() {
  assert!(parse_ecma_script("({ #m() {} });").is_err());
}

#[test]
fn object_literals_cannot_have_duplicate_proto_data_properties() {
  assert!(parse_ecma_script("({ __proto__: null, '__proto__': null });").is_err());
}

#[test]
fn private_identifiers_are_not_valid_assignment_targets() {
  assert!(parse_ecma_script("class C { #x; m() { for (#x in []) ; } }").is_err());
}

#[test]
fn do_while_cannot_have_semicolon_between_body_and_while() {
  assert!(parse_ecma_script("do {};\nwhile (false)").is_err());
}
