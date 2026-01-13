use crate::regexp_unicode_property_strings::{
  match_property_at, UnicodeStringProperty, MAX_MATCHES_PER_POSITION,
};

fn utf16(s: &str) -> Vec<u16> {
  s.encode_utf16().collect()
}

#[test]
fn prefix_matches_basic_vs_modifier_sequence() {
  // 👍 is Basic_Emoji and 👍🏻 is an RGI_Emoji_Modifier_Sequence. The union property must surface
  // both matches.
  let haystack = utf16("\u{1F44D}\u{1F3FB}");
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let n = match_property_at(UnicodeStringProperty::RgiEmoji, &haystack, 0, &mut out);
  assert_eq!(&out[..n], &[2, 4]);

  let n = match_property_at(UnicodeStringProperty::BasicEmoji, &haystack, 0, &mut out);
  assert_eq!(&out[..n], &[2]);

  let n =
    match_property_at(UnicodeStringProperty::RgiEmojiModifierSequence, &haystack, 0, &mut out);
  assert_eq!(&out[..n], &[4]);
}

#[test]
fn prefix_matches_basic_vs_zwj_sequence() {
  // 🏳️ is Basic_Emoji and 🏳️‍🌈 is an RGI_Emoji_ZWJ_Sequence.
  let haystack = utf16("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}");
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let n = match_property_at(UnicodeStringProperty::RgiEmoji, &haystack, 0, &mut out);
  assert_eq!(&out[..n], &[3, 6]);

  let n = match_property_at(UnicodeStringProperty::RgiEmojiZwjSequence, &haystack, 0, &mut out);
  assert_eq!(&out[..n], &[6]);
}

#[test]
fn prefix_matches_basic_vs_tag_sequence() {
  // 🏴 is Basic_Emoji and the England/Scotland/Wales flags are RGI_Emoji_Tag_Sequences that begin
  // with 🏴.
  let england = utf16("\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}");
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let n = match_property_at(UnicodeStringProperty::RgiEmoji, &england, 0, &mut out);
  assert_eq!(&out[..n], &[2, 14]);

  let n = match_property_at(UnicodeStringProperty::RgiEmojiTagSequence, &england, 0, &mut out);
  assert_eq!(&out[..n], &[14]);

  // Missing cancel tag => not a tag sequence, but still matches 🏴 as a Basic_Emoji.
  let incomplete = utf16("\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}");
  let n = match_property_at(
    UnicodeStringProperty::RgiEmojiTagSequence,
    &incomplete,
    0,
    &mut out,
  );
  assert_eq!(n, 0);
}

#[test]
fn non_matches() {
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let haystack = utf16("A");
  let n = match_property_at(UnicodeStringProperty::RgiEmoji, &haystack, 0, &mut out);
  assert_eq!(n, 0);

  // Verify `start` offset handling.
  let mut haystack = utf16("x");
  haystack.extend(utf16("\u{1F44D}\u{1F3FB}"));
  let n = match_property_at(UnicodeStringProperty::RgiEmoji, &haystack, 1, &mut out);
  assert_eq!(&out[..n], &[2, 4]);
}

