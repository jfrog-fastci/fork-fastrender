use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::BackgroundAttachment;
use fastrender::style::types::ImageRendering;
use fastrender::style::types::ObjectFit;
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
fn visual_property_keywords_are_ascii_case_insensitive() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background", "FIXED"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.background_attachments[0], BackgroundAttachment::Fixed);

  apply_declaration(
    &mut styles,
    &decl("margin", "10px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.margin_top, Some(Length::px(10.0)));

  apply_declaration(
    &mut styles,
    &decl("margin", "AUTO"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.margin_top, None);
  assert_eq!(styles.margin_right, None);
  assert_eq!(styles.margin_bottom, None);
  assert_eq!(styles.margin_left, None);

  apply_declaration(
    &mut styles,
    &decl("letter-spacing", "2px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!((styles.letter_spacing - 2.0).abs() < 1e-6);
  apply_declaration(
    &mut styles,
    &decl("letter-spacing", "NORMAL"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!((styles.letter_spacing - 0.0).abs() < 1e-6);

  apply_declaration(
    &mut styles,
    &decl("word-spacing", "2px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!((styles.word_spacing - 2.0).abs() < 1e-6);
  apply_declaration(
    &mut styles,
    &decl("word-spacing", "NORMAL"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!((styles.word_spacing - 0.0).abs() < 1e-6);

  apply_declaration(
    &mut styles,
    &decl("box-shadow", "1px 1px red"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(!styles.box_shadow.is_empty());
  apply_declaration(
    &mut styles,
    &decl("box-shadow", "NONE"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(styles.box_shadow.is_empty());

  apply_declaration(
    &mut styles,
    &decl("text-shadow", "1px 1px red"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(!styles.text_shadow.is_empty());
  apply_declaration(
    &mut styles,
    &decl("text-shadow", "NONE"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(styles.text_shadow.is_empty());

  apply_declaration(
    &mut styles,
    &decl("transform-origin", "LEFT TOP"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.transform_origin.x, Length::percent(0.0));
  assert_eq!(styles.transform_origin.y, Length::percent(0.0));

  apply_declaration(
    &mut styles,
    &decl("perspective-origin", "RIGHT BOTTOM"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.perspective_origin.x, Length::percent(100.0));
  assert_eq!(styles.perspective_origin.y, Length::percent(100.0));

  apply_declaration(
    &mut styles,
    &decl("object-fit", "COVER"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.object_fit, ObjectFit::Cover);

  apply_declaration(
    &mut styles,
    &decl("image-rendering", "PIXELATED"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.image_rendering, ImageRendering::Pixelated);
}

