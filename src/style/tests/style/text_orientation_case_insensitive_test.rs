use crate::css::types::Declaration;
use crate::css::types::PropertyValue;
use crate::style::properties::apply_declaration;
use crate::style::types::TextOrientation;
use crate::style::ComputedStyle;

#[test]
fn text_orientation_keywords_are_case_insensitive() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &Declaration {
      property: "text-orientation".into(),
      value: PropertyValue::Keyword("UPRIGHT".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.text_orientation, TextOrientation::Upright);

  apply_declaration(
    &mut style,
    &Declaration {
      property: "text-orientation".into(),
      value: PropertyValue::Keyword("SIDEWAYS-LEFT".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.text_orientation, TextOrientation::SidewaysLeft);
}
