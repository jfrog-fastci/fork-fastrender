use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::BackgroundImage;
use fastrender::style::ComputedStyle;

#[test]
fn background_layer_lists_reject_empty_items() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &Declaration {
      property: "background-image".into(),
      value: parse_property_value("background-image", "url(before.png)")
        .expect("parse background-image"),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(matches!(
    style.background_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url == "before.png"
  ));

  // Trailing commas are invalid in CSS comma-separated lists; ensure we do not treat them as
  // "ignore the empty last layer".
  apply_declaration(
    &mut style,
    &Declaration {
      property: "background-image".into(),
      value: parse_property_value("background-image", "url(after.png),")
        .expect("parse background-image"),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(matches!(
    style.background_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url == "before.png"
  ));

  // A well-formed list should still apply.
  apply_declaration(
    &mut style,
    &Declaration {
      property: "background-image".into(),
      value: parse_property_value("background-image", "url(a.png), url(b.png)")
        .expect("parse background-image"),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.background_layers.len(), 2);
  assert!(matches!(
    style.background_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url == "a.png"
  ));
  assert!(matches!(
    style.background_layers[1].image,
    Some(BackgroundImage::Url(ref url)) if url == "b.png"
  ));

  // Empty middle items are also invalid and should not clobber the previous valid value.
  apply_declaration(
    &mut style,
    &Declaration {
      property: "background-image".into(),
      value: parse_property_value("background-image", "url(c.png), , url(d.png)")
        .expect("parse background-image"),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.background_layers.len(), 2);
  assert!(matches!(
    style.background_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url == "a.png"
  ));
  assert!(matches!(
    style.background_layers[1].image,
    Some(BackgroundImage::Url(ref url)) if url == "b.png"
  ));
}

