use std::cmp::Ordering;

use vm_js::cmp_utf16;
use vm_js::ImportAttribute;
use vm_js::JsString;
use vm_js::ModuleRequest;

fn req(specifier: &str, attrs: Vec<ImportAttribute>) -> ModuleRequest {
  ModuleRequest::new(JsString::from_str(specifier).unwrap(), attrs)
}

#[test]
fn module_request_canonicalizes_attribute_order() {
  let a_type = ImportAttribute::try_new("type", "json").unwrap();
  let a_mode = ImportAttribute::try_new("mode", "strict").unwrap();

  let left = req("./foo.mjs", vec![a_type.clone(), a_mode.clone()]);
  let right = req("./foo.mjs", vec![a_mode, a_type]);

  // `ModuleRequestsEqual` semantics.
  assert!(left.spec_equal(&right));
  // Because `ModuleRequest::new` canonicalizes ordering, derived equality is compatible too.
  assert_eq!(left, right);
}

#[test]
fn module_request_not_equal_with_different_attribute_count() {
  let a_type = ImportAttribute::try_new("type", "json").unwrap();
  let a_mode = ImportAttribute::try_new("mode", "strict").unwrap();

  let left = req("./foo.mjs", vec![a_type.clone()]);
  let right = req("./foo.mjs", vec![a_type, a_mode]);

  assert!(!left.spec_equal(&right));
  assert_ne!(left, right);
}

#[test]
fn cmp_utf16_orders_by_utf16_code_units() {
  // 😀 (U+1F600) is encoded as a surrogate pair in UTF-16: [0xD83D, 0xDE00].
  // U+E000 is a BMP code point encoded as [0xE000].
  //
  // By UTF-16 code unit ordering: 0xD83D < 0xE000 => 😀 < U+E000.
  assert_eq!(cmp_utf16("😀", "\u{E000}"), Ordering::Less);

  // Rust's default `str` ordering is based on UTF-8 bytes, where 0xF0 > 0xEE => 😀 > U+E000.
  assert_eq!("😀".cmp("\u{E000}"), Ordering::Greater);
}
