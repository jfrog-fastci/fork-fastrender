/// UTF-16 utilities.
///
/// ECMAScript strings are sequences of UTF-16 code units. Some contexts in the
/// language require the code unit sequence to be a *well-formed* UTF-16 encoding
/// of Unicode scalar values (i.e. no unpaired surrogates).
///
/// See: https://tc39.es/ecma262/#sec-isstringwellformedunicode
#[inline]
pub fn is_string_well_formed_unicode(code_units: &[u16]) -> bool {
  let mut i = 0;
  while i < code_units.len() {
    let unit = code_units[i];
    // High surrogate.
    if (0xD800..=0xDBFF).contains(&unit) {
      let Some(&next) = code_units.get(i + 1) else {
        return false;
      };
      if !(0xDC00..=0xDFFF).contains(&next) {
        return false;
      }
      i += 2;
      continue;
    }
    // Low surrogate without preceding high surrogate.
    if (0xDC00..=0xDFFF).contains(&unit) {
      return false;
    }
    i += 1;
  }
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn string_well_formed_unicode_accepts_surrogate_pairs() {
    assert!(is_string_well_formed_unicode(&[0xD83C, 0xDF19])); // 🌙
  }

  #[test]
  fn string_well_formed_unicode_rejects_unpaired_high_surrogate() {
    assert!(!is_string_well_formed_unicode(&[0xD83C]));
  }

  #[test]
  fn string_well_formed_unicode_rejects_unpaired_low_surrogate() {
    assert!(!is_string_well_formed_unicode(&[0xDF19]));
  }
}
