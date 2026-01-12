use crate::css::types::Declaration;
use crate::css::types::PropertyValue;
use crate::style::properties::apply_declaration;
use crate::style::types::TextCombineUpright;
use crate::style::ComputedStyle;

#[test]
fn text_combine_upright_rejects_out_of_range_digits() {
  let mut style = ComputedStyle::default();
  apply_declaration(
    &mut style,
    &Declaration {
      property: "text-combine-upright".into(),
      value: PropertyValue::Multiple(vec![
        PropertyValue::Keyword("digits".into()),
        PropertyValue::Number(5.0),
      ]),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    },
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.text_combine_upright, TextCombineUpright::None);
}
