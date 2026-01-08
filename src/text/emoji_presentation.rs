use crate::style::types::FontVariantEmoji;
use crate::text::bidi_controls::is_bidi_format_char;
use crate::text::emoji;
use crate::text::font_db::{FontDatabase, LoadedFont};
use crate::text::font_fallback::EmojiPreference;
use fontdb::ID;

pub(crate) fn font_is_emoji_font(db: &FontDatabase, id: Option<ID>, font: &LoadedFont) -> bool {
  let has_color_tables = match id {
    Some(id) => db.is_color_capable_font(id),
    None => crate::text::face_cache::with_face(font, crate::text::font_db::face_has_color_tables),
  };

  // `is_color_capable_font` can return `Some(false)` for monochrome emoji fonts (e.g. Symbola).
  // Emoji font classification should treat the presence of color tables as a strong signal, but
  // still fall back to family-name heuristics for emoji fonts that aren't color-capable.
  if matches!(has_color_tables, Some(true)) {
    return true;
  }

  FontDatabase::family_name_is_emoji_font(&font.family)
}

pub(crate) fn emoji_preference_for_char(ch: char, variant: FontVariantEmoji) -> EmojiPreference {
  // `font-variant-emoji` only applies to emoji codepoints; it should not bias font selection for
  // ordinary text. Keep non-emoji characters neutral regardless of property value.
  if !emoji::is_emoji(ch) && !emoji::is_emoji_presentation(ch) {
    return EmojiPreference::Neutral;
  }

  match variant {
    FontVariantEmoji::Emoji => EmojiPreference::PreferEmoji,
    FontVariantEmoji::Text => EmojiPreference::AvoidEmoji,
    FontVariantEmoji::Unicode => {
      if emoji::is_emoji_presentation(ch) {
        EmojiPreference::PreferEmoji
      } else {
        EmojiPreference::AvoidEmoji
      }
    }
    FontVariantEmoji::Normal => {
      if emoji::is_emoji_presentation(ch) {
        EmojiPreference::PreferEmoji
      } else {
        EmojiPreference::Neutral
      }
    }
  }
}

pub(crate) fn emoji_preference_with_selector(
  ch: char,
  next: Option<char>,
  variant: FontVariantEmoji,
) -> EmojiPreference {
  if let Some(sel) = next {
    if sel == '\u{FE0F}' {
      return EmojiPreference::PreferEmoji;
    }
    if sel == '\u{FE0E}' {
      return EmojiPreference::AvoidEmoji;
    }

    // Keycap sequences (e.g. 1️⃣, #️⃣) can omit VS16 but still default to emoji presentation.
    if sel == '\u{20E3}' && emoji::is_keycap_base(ch) {
      return EmojiPreference::PreferEmoji;
    }
  }

  let base_pref = emoji_preference_for_char(ch, variant);

  // ZWJ sequences are emoji presentation even if the base codepoint has a text default.
  if let Some('\u{200d}') = next {
    if emoji::is_emoji(ch) || emoji::is_emoji_presentation(ch) {
      return EmojiPreference::PreferEmoji;
    }
  }

  base_pref
}

pub(crate) fn emoji_preference_for_cluster(
  cluster_text: &str,
  variant: FontVariantEmoji,
) -> EmojiPreference {
  let mut chars = cluster_text.chars();
  let Some(first) = chars.next() else {
    return EmojiPreference::Neutral;
  };
  if chars.as_str().is_empty() {
    return emoji_preference_for_char(first, variant);
  }

  let mut base_char: Option<char> = None;
  let mut prev_renderable: Option<char> = None;
  let mut saw_vs15 = false;
  let mut saw_vs16 = false;
  let mut has_zwj = false;
  let mut has_keycap = false;
  let mut has_tag_chars = false;
  let mut has_emoji = false;

  for ch in cluster_text.chars() {
    let cp = ch as u32;
    match cp {
      0x200d => has_zwj = true,
      0x20e3 => has_keycap = true,
      0xfe0e => {
        if prev_renderable.is_some() {
          saw_vs15 = true;
        }
      }
      0xfe0f => {
        if prev_renderable.is_some() {
          saw_vs16 = true;
        }
      }
      _ => {}
    }

    if (0xe0020..=0xe007f).contains(&cp) {
      has_tag_chars = true;
    }

    if !is_non_rendering_for_preference(ch) {
      prev_renderable = Some(ch);
      base_char.get_or_insert(ch);
      if emoji::is_emoji(ch) || emoji::is_emoji_presentation(ch) {
        has_emoji = true;
      }
    }
  }

  // Explicit variation selectors always win.
  if saw_vs15 {
    return EmojiPreference::AvoidEmoji;
  }
  if saw_vs16 {
    return EmojiPreference::PreferEmoji;
  }

  // Keycap sequences render as emoji even without an explicit VS16 (UAX #51).
  if emoji::is_keycap_base(first) && has_keycap {
    return EmojiPreference::PreferEmoji;
  }

  // Emoji tag sequences (subdivision flags).
  if first == '\u{1F3F4}' && has_tag_chars {
    return EmojiPreference::PreferEmoji;
  }

  // ZWJ sequences that involve emoji should prefer emoji fonts even under
  // `font-variant-emoji: text`.
  if has_zwj && has_emoji {
    return EmojiPreference::PreferEmoji;
  }

  emoji_preference_for_char(base_char.unwrap_or(first), variant)
}

fn is_non_rendering_for_preference(ch: char) -> bool {
  is_bidi_format_char(ch)
    || matches!(ch, '\u{200c}' | '\u{200d}')
    || ('\u{fe00}'..='\u{fe0f}').contains(&ch)
    || ('\u{e0100}'..='\u{e01ef}').contains(&ch)
    || ('\u{180b}'..='\u{180d}').contains(&ch)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::{font_resolver, pipeline};

  #[test]
  fn emoji_variation_selectors_override_property_preference() {
    assert_eq!(
      emoji_preference_with_selector('😀', Some('\u{fe0e}'), FontVariantEmoji::Emoji),
      EmojiPreference::AvoidEmoji
    );
    assert_eq!(
      emoji_preference_with_selector('😀', Some('\u{fe0f}'), FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn zwj_sequences_prefer_emoji_fonts() {
    assert_eq!(
      emoji_preference_with_selector('👩', Some('\u{200d}'), FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn non_emoji_characters_are_neutral() {
    for variant in [
      FontVariantEmoji::Normal,
      FontVariantEmoji::Emoji,
      FontVariantEmoji::Text,
      FontVariantEmoji::Unicode,
    ] {
      assert_eq!(
        emoji_preference_for_char('A', variant),
        EmojiPreference::Neutral,
        "expected non-emoji characters to remain neutral for {variant:?}"
      );
      assert_eq!(
        emoji_preference_with_selector('A', None, variant),
        EmojiPreference::Neutral,
        "expected non-emoji characters to remain neutral for {variant:?}"
      );
    }
  }

  #[test]
  fn keycap_sequences_without_vs16_default_to_emoji() {
    assert_eq!(
      emoji_preference_with_selector('1', Some('\u{20e3}'), FontVariantEmoji::Normal),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_with_selector('#', Some('\u{20e3}'), FontVariantEmoji::Unicode),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_with_selector('1', Some('\u{20e3}'), FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn cluster_preference_matches_selector_rules() {
    assert_eq!(
      emoji_preference_for_cluster("1\u{20e3}", FontVariantEmoji::Normal),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_for_cluster("😀\u{fe0e}", FontVariantEmoji::Emoji),
      EmojiPreference::AvoidEmoji
    );
  }

  #[test]
  fn cluster_prefers_emoji_for_tag_and_complex_zwj_sequences() {
    assert_eq!(
      emoji_preference_for_cluster("👩\u{1f3fb}\u{200d}🔬", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );

    // England subdivision flag tag sequence.
    let england = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}";
    assert_eq!(
      emoji_preference_for_cluster(england, FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn pipeline_and_font_resolver_preferences_match() {
    let cases = [
      ('😀', None, FontVariantEmoji::Normal),
      ('😀', Some('\u{fe0e}'), FontVariantEmoji::Emoji),
      ('😀', Some('\u{fe0f}'), FontVariantEmoji::Text),
      ('👩', Some('\u{200d}'), FontVariantEmoji::Text),
      ('A', None, FontVariantEmoji::Unicode),
      ('1', Some('\u{20e3}'), FontVariantEmoji::Normal),
      ('1', Some('\u{20e3}'), FontVariantEmoji::Text),
    ];

    for (ch, next, variant) in cases {
      let pipeline_pref = pipeline::emoji_preference_with_selector(ch, next, variant);
      let resolver_pref = font_resolver::emoji_preference_with_selector(ch, next, variant);
      assert_eq!(
        pipeline_pref, resolver_pref,
        "emoji preference mismatch for {ch:?} + {next:?} ({variant:?})"
      );
    }

    let char_cases = [
      ('😀', FontVariantEmoji::Normal),
      ('#', FontVariantEmoji::Unicode),
      ('A', FontVariantEmoji::Emoji),
    ];
    for (ch, variant) in char_cases {
      let pipeline_pref = pipeline::emoji_preference_for_char(ch, variant);
      let resolver_pref = font_resolver::emoji_preference_for_char(ch, variant);
      assert_eq!(
        pipeline_pref, resolver_pref,
        "emoji preference mismatch for char {ch:?} ({variant:?})"
      );
    }
  }
}
