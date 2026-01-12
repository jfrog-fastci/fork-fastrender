use crate::css::types::PropertyValue;
use crate::geometry::Size;
use crate::style::types::LineHeight;
use crate::style::values::{CustomPropertyValue, LengthUnit};
use crate::style::{ComputedStyle, VarDependentDeclaration};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn recompute_var_dependent_properties_resolves_line_height_lengths() {
  let parent_styles = ComputedStyle::default();
  let viewport = Size::new(800.0, 600.0);

  let mut styles = ComputedStyle::default();
  styles.font_size = 10.0;
  styles.root_font_size = 10.0;

  styles
    .custom_properties
    .insert(Arc::from("--lh"), CustomPropertyValue::new("2em", None));

  let mut deps = HashMap::new();
  deps.insert(
    "line-height",
    VarDependentDeclaration {
      order: 0,
      value: PropertyValue::Custom("var(--lh)".into()),
    },
  );
  styles.var_dependent_declarations = Arc::new(deps);

  styles.recompute_var_dependent_properties(&parent_styles, viewport);

  match styles.line_height {
    LineHeight::Length(len) => {
      assert_eq!(len.unit, LengthUnit::Px);
      assert!(
        (len.value - 20.0).abs() < 0.01,
        "expected 20px, got {len:?}"
      );
    }
    other => panic!("expected LineHeight::Length after recompute, got {other:?}"),
  }
}
