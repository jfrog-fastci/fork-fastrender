use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

fn parse_ok(src: &str) {
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

fn parse_err(src: &str) {
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn rejects_unicode_mode_non_single_class_range_endpoints() {
  parse_err(r#"let r = /[--\d]/u;"#);
  parse_err(r#"let r = /[\d-a]/u;"#);
  parse_err(r#"let r = /[%-\d]/u;"#);
  parse_err(r#"let r = /[\s-\d]/u;"#);
}

#[test]
fn accepts_unicode_mode_braced_class_escapes() {
  parse_ok(r#"let r = /[\u{41}]/u;"#);
  parse_ok(r#"let r = /[\u{1F438}]/u;"#);
  parse_ok(r#"let r = /[\u{1F418}-\u{1F438}]/u;"#);
}

#[test]
fn rejects_invalid_unicode_mode_braced_class_escapes() {
  parse_err(r#"let r = /[\u{}]/u;"#);
  parse_err(r#"let r = /[\u{]/u;"#);
  parse_err(r#"let r = /[\u{G}]/u;"#);
  parse_err(r#"let r = /[\u{110000}]/u;"#);
  parse_err(r#"let r = /[\u{FF FF}]/u;"#);
}

#[test]
fn rejects_out_of_order_class_ranges_all_modes() {
  parse_err(r#"let r = /[z-a]/;"#);
  parse_err(r#"let r = /[\u{1F438}-\u{1F418}]/u;"#);
}

#[test]
fn rejects_invalid_unicode_sets_mode_braced_class_escapes() {
  parse_err(r#"let r = /[\u{}]/v;"#);
  parse_err(r#"let r = /[\u{110000}]/v;"#);
  parse_err(r#"let r = /[\q{\u{110000}}]/v;"#);
}

#[test]
fn rejects_out_of_order_class_ranges_in_unicode_sets_mode() {
  parse_err(r#"let r = /[z-a]/v;"#);
}
