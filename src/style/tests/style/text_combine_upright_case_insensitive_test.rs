use crate::css::types::Declaration;
use crate::css::types::PropertyValue;
use crate::style::properties::apply_declaration;
use crate::style::types::TextCombineUpright;
use crate::style::ComputedStyle;

#[test]
fn text_combine_upright_keywords_are_case_insensitive() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &Declaration {
      property: "text-combine-upright".into(),
      value: PropertyValue::Keyword("DIGITS".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.text_combine_upright, TextCombineUpright::Digits(2));

  apply_declaration(
    &mut style,
    &Declaration {
      property: "text-combine-upright".into(),
      value: PropertyValue::Keyword("ALL".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(style.text_combine_upright, TextCombineUpright::All);
}
