use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

fn parses_ecma(src: &str) -> bool {
  parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    },
  )
  .is_ok()
}

#[test]
fn parses_valid_unicode_property_escape() {
  assert!(parses_ecma(r"/(?:\p{Lu})/u;"));
}

#[test]
fn parses_valid_unicode_property_name_value_escape() {
  // Non-binary property with explicit value.
  assert!(parses_ecma(r"/\p{Script=Greek}/u;"));
  assert!(parses_ecma(r"/\p{General_Category=Uppercase_Letter}/u;"));
}

#[test]
fn rejects_binary_property_with_value() {
  assert!(!parses_ecma(r"/\p{ASCII=N}/u;"));
}

#[test]
fn rejects_loose_matching() {
  assert!(!parses_ecma(r"/\P{General_Category = Uppercase_Letter}/u;"));
}

#[test]
fn rejects_nonbinary_property_without_value() {
  assert!(!parses_ecma(r"/\P{Script=}/u;"));
}

#[test]
fn rejects_empty_body() {
  assert!(!parses_ecma(r"/\p{}/u;"));
}

#[test]
fn rejects_unclosed_body() {
  assert!(!parses_ecma(r"/\p{/u;"));
}

#[test]
fn rejects_property_escape_as_range_endpoint_in_unicode_mode() {
  // test262: built-ins/RegExp/property-escapes/character-class-range-*.js
  assert!(!parses_ecma(r"/[\p{Hex}--]/u;"));
  assert!(!parses_ecma(r"/[--\p{Hex}]/u;"));
  assert!(!parses_ecma(r"/[\p{Hex}-\uFFFF]/u;"));
  assert!(!parses_ecma(r"/[\uFFFF-\p{Hex}]/u;"));
}
