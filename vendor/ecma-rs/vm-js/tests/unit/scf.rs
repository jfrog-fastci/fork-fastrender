use crate::unicode_case_folding::scf;

#[test]
fn ascii_sanity() {
  assert_eq!(scf('A' as u32), 'a' as u32);
  assert_eq!(scf('a' as u32), 'a' as u32);
}

#[test]
fn non_ascii_sanity() {
  // U+00B5 MICRO SIGN => U+03BC GREEK SMALL LETTER MU
  assert_eq!(scf(0x00B5), 0x03BC);
}

#[test]
fn surrogate_sanity() {
  assert_eq!(scf(0xD800), 0xD800);
}

