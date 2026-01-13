use vm_js::regexp_case_fold;

#[test]
fn regexp_case_folding_includes_common_mappings() {
  // U+212A KELVIN SIGN (K) -> U+006B 'k'
  assert_eq!(regexp_case_fold(0x212A), 0x006B);
  // U+2126 OHM SIGN (Ω) -> U+03C9 GREEK SMALL LETTER OMEGA (ω)
  assert_eq!(regexp_case_fold(0x2126), 0x03C9);
  // U+017F LATIN SMALL LETTER LONG S (ſ) -> U+0073 's'
  assert_eq!(regexp_case_fold(0x017F), 0x0073);
}

#[test]
fn regexp_case_folding_ignores_full_mappings() {
  // U+00DF LATIN SMALL LETTER SHARP S (ß) has a *full* fold to "ss", but no simple/common fold.
  // RegExp `Canonicalize`/`scf` must therefore not expand it into multiple code points.
  assert_eq!(regexp_case_fold(0x00DF), 0x00DF);
}

