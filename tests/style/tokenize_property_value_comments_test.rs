use fastrender::css::properties::parse_property_value;
use fastrender::css::types::PropertyValue;
use fastrender::style::color::Rgba;
use fastrender::style::values::Length;

#[test]
fn tokenize_property_value_skips_css_comments() {
  // CSS comments act like whitespace, so they should not change tokenization for values that are
  // parsed by splitting on top-level separators.
  let parsed = parse_property_value("text-shadow", "1px 2px/*comment*/red")
    .expect("text-shadow with comment should parse");

  let PropertyValue::TextShadow(shadows) = parsed else {
    panic!("expected text-shadow value, got {parsed:?}");
  };

  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert_eq!(shadow.offset_x, Length::px(1.0));
  assert_eq!(shadow.offset_y, Length::px(2.0));
  assert_eq!(shadow.blur_radius, Length::px(0.0));
  assert_eq!(shadow.color, Some(Rgba::RED));
}

