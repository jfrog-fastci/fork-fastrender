pub(crate) use crate::regexp_unicode_property_strings::UnicodeStringProperty;

/// Resolve the `\p{...}` / `\P{...}` property name for Unicode **string properties** used by `/v`.
///
/// ECMA-262 treats Unicode property matching as case-insensitive and (for these names) generally
/// ignores `_`, `-`, and ASCII whitespace.
///
/// This helper is intentionally minimal: it only handles the string properties that `vm-js`
/// implements (emoji sequences), and does **not** attempt full UnicodePropertyAliases.txt support.
pub(crate) fn resolve_unicode_string_property(name: &str) -> Option<UnicodeStringProperty> {
  // Normalized names are tiny (< 32 bytes) for the supported properties, so a small stack buffer
  // avoids heap allocation.
  let mut buf = [0u8; 64];
  let mut len = 0usize;

  for b in name.bytes() {
    let b = match b {
      b'A'..=b'Z' => b + 0x20,
      b'_' | b'-' | b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b' => continue,
      other => other,
    };
    if len == buf.len() {
      // The supported properties are short; reject pathological inputs without allocating.
      return None;
    }
    buf[len] = b;
    len += 1;
  }

  let normalized = &buf[..len];

  if normalized == b"basicemoji" {
    return Some(UnicodeStringProperty::BasicEmoji);
  }
  if normalized == b"emojikeycapsequence" {
    return Some(UnicodeStringProperty::EmojiKeycapSequence);
  }
  if normalized == b"rgiemojiflagsequence" {
    return Some(UnicodeStringProperty::RgiEmojiFlagSequence);
  }
  if normalized == b"rgiemojimodifiersequence" {
    return Some(UnicodeStringProperty::RgiEmojiModifierSequence);
  }
  if normalized == b"rgiemojitagsequence" {
    return Some(UnicodeStringProperty::RgiEmojiTagSequence);
  }
  if normalized == b"rgiemojizwjsequence" {
    return Some(UnicodeStringProperty::RgiEmojiZwjSequence);
  }
  if normalized == b"rgiemoji" {
    return Some(UnicodeStringProperty::RgiEmoji);
  }

  None
}

#[cfg(test)]
mod tests {
  use crate::regexp::{resolve_unicode_string_property, UnicodeStringProperty};

  #[test]
  fn resolves_unicode_string_properties_with_separators_and_case_insensitive() {
    let cases: &[(UnicodeStringProperty, &[&str])] = &[
      (
        UnicodeStringProperty::BasicEmoji,
        &["Basic_Emoji", "basic_emoji", "basic-emoji", "Basic Emoji"],
      ),
      (
        UnicodeStringProperty::EmojiKeycapSequence,
        &[
          "Emoji_Keycap_Sequence",
          "emoji_keycap_sequence",
          "emoji-keycap-sequence",
          "Emoji Keycap_Sequence",
        ],
      ),
      (
        UnicodeStringProperty::RgiEmojiFlagSequence,
        &[
          "RGI_Emoji_Flag_Sequence",
          "rgi_emoji_flag_sequence",
          "rgi-emoji-flag-sequence",
          "RGI Emoji Flag_Sequence",
        ],
      ),
      (
        UnicodeStringProperty::RgiEmojiModifierSequence,
        &[
          "RGI_Emoji_Modifier_Sequence",
          "rgi_emoji_modifier_sequence",
          "rgi-emoji-modifier-sequence",
          "RGI Emoji Modifier_Sequence",
        ],
      ),
      (
        UnicodeStringProperty::RgiEmojiTagSequence,
        &[
          "RGI_Emoji_Tag_Sequence",
          "rgi_emoji_tag_sequence",
          "rgi-emoji-tag-sequence",
          "RGI Emoji Tag_Sequence",
        ],
      ),
      (
        UnicodeStringProperty::RgiEmojiZwjSequence,
        &[
          "RGI_Emoji_ZWJ_Sequence",
          "rgi_emoji_zwj_sequence",
          "rgi-emoji-zwj-sequence",
          "RGI Emoji ZWJ_Sequence",
        ],
      ),
      (
        UnicodeStringProperty::RgiEmoji,
        &["RGI_Emoji", "rgi_emoji", "rgi-emoji", "RGI emoji"],
      ),
    ];

    for (expected, inputs) in cases {
      for input in *inputs {
        assert_eq!(
          resolve_unicode_string_property(input),
          Some(*expected),
          "input {input:?} should resolve"
        );
      }
    }
  }

  #[test]
  fn unknown_unicode_string_property_returns_none() {
    assert_eq!(resolve_unicode_string_property("not_a_property"), None);
    assert_eq!(resolve_unicode_string_property("emoji"), None);
    assert_eq!(resolve_unicode_string_property("rgi_emoji_unknown"), None);
  }
}
