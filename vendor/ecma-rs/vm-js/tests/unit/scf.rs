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

#[test]
fn ignores_full_and_turkic_mappings() {
  // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE has only full (`F`) and Turkic (`T`) mappings in
  // Unicode CaseFolding.txt, so `scf` must treat it as identity.
  assert_eq!(scf(0x0130), 0x0130);
}
