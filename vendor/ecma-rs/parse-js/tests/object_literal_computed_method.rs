use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn parses_object_literal_computed_method_name() {
  // ECMA-262: object literal method with a computed property name.
  //
  // This shape is used by test262 for `@@toPrimitive`:
  //   { [Symbol.toPrimitive](hint) { ... } }
  let src = "({ [Symbol.toPrimitive](hint) { return hint; } })";
  parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    },
  )
  .unwrap();
}

