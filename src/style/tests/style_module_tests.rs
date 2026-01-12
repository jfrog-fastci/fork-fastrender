use crate::style::{ComputedStyle, MaskBorder};

#[test]
fn normalizes_language_tags_to_lower_hyphenated() {
  assert_eq!(crate::style::normalize_language_tag("En-US"), "en-us");
  assert_eq!(crate::style::normalize_language_tag(" sr_Cyrl_RS "), "sr-cyrl-rs");
  assert_eq!(crate::style::normalize_language_tag(""), "");
}

#[test]
fn non_ascii_whitespace_normalize_language_tag_does_not_trim_nbsp() {
  let nbsp = "\u{00A0}";
  assert_eq!(
    crate::style::normalize_language_tag(&format!("{nbsp}En-US")),
    format!("{nbsp}en-us")
  );
}

#[test]
fn reset_background_to_initial_resets_mask_border() {
  let mut style = ComputedStyle::default();
  style.mask_border.source = crate::style::types::BorderImageSource::Image(Box::new(
    crate::style::types::BackgroundImage::Url(crate::style::types::BackgroundImageUrl::new(
      "https://example.invalid/mask.png".to_string(),
    )),
  ));
  style.reset_background_to_initial();
  assert_eq!(style.mask_border, MaskBorder::default());
}
