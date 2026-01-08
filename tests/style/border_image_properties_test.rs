use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::{
  BackgroundImage, BorderImageOutset, BorderImageOutsetValue, BorderImageRepeat, BorderImageSlice,
  BorderImageSliceValue, BorderImageSource, BorderImageWidth, BorderImageWidthValue,
};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;

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
fn border_image_slice_parses_numbers_and_fill() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image-slice", "30 30 fill"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.border_image.slice,
    BorderImageSlice {
      top: BorderImageSliceValue::Number(30.0),
      right: BorderImageSliceValue::Number(30.0),
      bottom: BorderImageSliceValue::Number(30.0),
      left: BorderImageSliceValue::Number(30.0),
      fill: true,
    }
  );
}

#[test]
fn border_image_slice_parses_percentages() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image-slice", "10% 20% 30% 40% fill"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.border_image.slice,
    BorderImageSlice {
      top: BorderImageSliceValue::Percentage(10.0),
      right: BorderImageSliceValue::Percentage(20.0),
      bottom: BorderImageSliceValue::Percentage(30.0),
      left: BorderImageSliceValue::Percentage(40.0),
      fill: true,
    }
  );
}

#[test]
fn border_image_width_parses_lengths() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image-width", "10px 20px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.border_image.width,
    BorderImageWidth {
      top: BorderImageWidthValue::Length(Length::px(10.0)),
      right: BorderImageWidthValue::Length(Length::px(20.0)),
      bottom: BorderImageWidthValue::Length(Length::px(10.0)),
      left: BorderImageWidthValue::Length(Length::px(20.0)),
    }
  );
}

#[test]
fn border_image_shorthand_splits_segments() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a) 30 / 10px / 0 stretch"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  match &styles.border_image.source {
    BorderImageSource::Image(img) => match &**img {
      BackgroundImage::Url(url) => assert_eq!(url, "a"),
      other => panic!("unexpected background image variant: {:?}", other),
    },
    other => panic!("unexpected border image source: {:?}", other),
  }

  assert_eq!(
    styles.border_image.slice,
    BorderImageSlice {
      top: BorderImageSliceValue::Number(30.0),
      right: BorderImageSliceValue::Number(30.0),
      bottom: BorderImageSliceValue::Number(30.0),
      left: BorderImageSliceValue::Number(30.0),
      fill: false,
    }
  );

  assert_eq!(
    styles.border_image.width,
    BorderImageWidth {
      top: BorderImageWidthValue::Length(Length::px(10.0)),
      right: BorderImageWidthValue::Length(Length::px(10.0)),
      bottom: BorderImageWidthValue::Length(Length::px(10.0)),
      left: BorderImageWidthValue::Length(Length::px(10.0)),
    }
  );

  assert_eq!(
    styles.border_image.outset,
    BorderImageOutset {
      top: BorderImageOutsetValue::Number(0.0),
      right: BorderImageOutsetValue::Number(0.0),
      bottom: BorderImageOutsetValue::Number(0.0),
      left: BorderImageOutsetValue::Number(0.0),
    }
  );

  assert_eq!(
    styles.border_image.repeat,
    (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch)
  );
}

#[test]
fn border_image_repeat_accepts_case_insensitive_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image-repeat", "ROUND"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.border_image.repeat,
    (BorderImageRepeat::Round, BorderImageRepeat::Round)
  );
}

#[test]
fn border_image_shorthand_accepts_case_insensitive_repeat_keywords() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a) 30 / 10px / 0 ROUND"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.border_image.repeat,
    (BorderImageRepeat::Round, BorderImageRepeat::Round)
  );
}

#[test]
fn border_image_shorthand_source_only_uses_initial_slice_and_width() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a)"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  match &styles.border_image.source {
    BorderImageSource::Image(img) => match &**img {
      BackgroundImage::Url(url) => assert_eq!(url, "a"),
      other => panic!("unexpected background image variant: {:?}", other),
    },
    other => panic!("unexpected border image source: {:?}", other),
  }

  assert_eq!(
    styles.border_image.slice,
    BorderImageSlice {
      top: BorderImageSliceValue::Percentage(100.0),
      right: BorderImageSliceValue::Percentage(100.0),
      bottom: BorderImageSliceValue::Percentage(100.0),
      left: BorderImageSliceValue::Percentage(100.0),
      fill: false,
    }
  );

  assert_eq!(
    styles.border_image.width,
    BorderImageWidth {
      top: BorderImageWidthValue::Number(1.0),
      right: BorderImageWidthValue::Number(1.0),
      bottom: BorderImageWidthValue::Number(1.0),
      left: BorderImageWidthValue::Number(1.0),
    }
  );
}

#[test]
fn border_image_slice_rejects_duplicate_fill() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-slice", "30 fill"), &parent, 16.0, 16.0);
  let expected = styles.border_image.slice.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image-slice", "30 fill fill"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.slice, expected);
}

#[test]
fn border_image_slice_rejects_too_many_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-slice", "30 fill"), &parent, 16.0, 16.0);
  let expected = styles.border_image.slice.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image-slice", "10 20 30 40 50 fill"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.slice, expected);
}

#[test]
fn border_image_width_rejects_too_many_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-width", "10px"), &parent, 16.0, 16.0);
  let expected = styles.border_image.width.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image-width", "1 2 3 4 5"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.width, expected);
}

#[test]
fn border_image_outset_rejects_too_many_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-outset", "1"), &parent, 16.0, 16.0);
  let expected = styles.border_image.outset.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image-outset", "1 2 3 4 5"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.outset, expected);
}

#[test]
fn border_image_repeat_rejects_too_many_keywords() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-repeat", "round"), &parent, 16.0, 16.0);
  let expected = styles.border_image.repeat;

  apply_declaration(
    &mut styles,
    &decl("border-image-repeat", "stretch round space"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.repeat, expected);
}

#[test]
fn border_image_repeat_rejects_non_keywords() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(&mut styles, &decl("border-image-repeat", "round"), &parent, 16.0, 16.0);
  let expected = styles.border_image.repeat;

  apply_declaration(
    &mut styles,
    &decl("border-image-repeat", "stretch 10px"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image.repeat, expected);
}

#[test]
fn border_image_shorthand_rejects_too_many_repeat_keywords() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a) 30 / 10px / 0 stretch"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.border_image.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(b) 30 / 10px / 0 stretch round space"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image, expected);
}

#[test]
fn border_image_shorthand_rejects_too_many_slashes() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a) 30 / 10px / 0 stretch"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.border_image.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(b) 30 / 10px / 0 / 1 stretch"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image, expected);
}

#[test]
fn border_image_shorthand_rejects_slashes_without_slice() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(a) 30 / 10px / 0 stretch"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.border_image.clone();

  apply_declaration(
    &mut styles,
    &decl("border-image", "url(b) / 10px / 0 stretch"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.border_image, expected);
}
