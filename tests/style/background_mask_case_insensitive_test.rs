use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::{
  BackgroundAttachment, BackgroundBox, BackgroundPosition, BackgroundRepeat, BackgroundSize,
  BackgroundSizeKeyword, MaskClip, MaskComposite, MaskMode, MaskOrigin,
};
use fastrender::style::values::Length;
use fastrender::ComputedStyle;

fn decl(name: &'static str, value: &str) -> Declaration {
  let contains_var = fastrender::style::var_resolution::contains_var(value);
  Declaration {
    property: name.into(),
    value: parse_property_value(name, value).expect("parse property value"),
    contains_var,
    raw_value: value.to_string(),
    important: false,
  }
}

#[test]
fn background_clip_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-clip", "TEXT"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_clips[0], BackgroundBox::Text);
}

#[test]
fn webkit_background_clip_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("-webkit-background-clip", "TEXT"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_clips[0], BackgroundBox::Text);
}

#[test]
fn background_origin_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-origin", "CONTENT-BOX"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_origins[0], BackgroundBox::ContentBox);
}

#[test]
fn background_repeat_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-repeat", "NO-REPEAT"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_repeats[0], BackgroundRepeat::no_repeat());
}

#[test]
fn background_repeat_x_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-repeat", "REPEAT-X"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_repeats[0], BackgroundRepeat::repeat_x());
}

#[test]
fn background_size_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-size", "COVER"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.background_sizes[0],
    BackgroundSize::Keyword(BackgroundSizeKeyword::Cover)
  );
}

#[test]
fn background_attachment_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-attachment", "FIXED"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.background_attachments[0], BackgroundAttachment::Fixed);
}

#[test]
fn background_position_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-position", "LEFT TOP"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  match styles.background_positions[0] {
    BackgroundPosition::Position { x, y } => {
      assert_eq!(x.alignment, 0.0);
      assert_eq!(x.offset, Length::percent(0.0));
      assert_eq!(y.alignment, 0.0);
      assert_eq!(y.offset, Length::percent(0.0));
    }
  }
}

#[test]
fn mask_mode_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask-mode", "LUMINANCE"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.mask_modes[0], MaskMode::Luminance);
}

#[test]
fn mask_origin_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask-origin", "CONTENT-BOX"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.mask_origins[0], MaskOrigin::ContentBox);
}

#[test]
fn mask_clip_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask-clip", "NO-CLIP"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.mask_clips[0], MaskClip::NoClip);
}

#[test]
fn mask_composite_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask-composite", "XOR"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.mask_composites[0], MaskComposite::Exclude);
}
