use crate::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn accessor_is_not_reserved_in_ecma_script() {
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };

  assert!(
    parse_with_options("var accessor = 1;", opts).is_ok(),
    "`accessor` should be usable as a binding identifier"
  );
  assert!(
    parse_with_options("'use strict'; var accessor = 1;", opts).is_ok(),
    "`accessor` should be usable as a binding identifier in strict mode"
  );
}

