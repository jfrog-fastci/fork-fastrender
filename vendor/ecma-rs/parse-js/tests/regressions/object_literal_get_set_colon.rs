use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn object_literal_get_set_can_be_plain_properties() {
  let source = "const handler = { set: () => false, get: (target, key) => target[key] };";
  for dialect in [Dialect::Ecma, Dialect::Ts] {
    parse_with_options(
      source,
      ParseOptions {
        dialect,
        source_type: SourceType::Module,
      },
    )
    .unwrap();
  }
}

