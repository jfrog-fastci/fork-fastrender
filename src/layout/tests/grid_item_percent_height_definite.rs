use crate::css::types::Declaration;
use crate::css::types::PropertyValue;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::style::display::Display;
use crate::style::properties::apply_declaration;
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};

fn decl(name: &'static str, value: PropertyValue) -> Declaration {
  let contains_var = match &value {
    PropertyValue::Keyword(raw) | PropertyValue::Custom(raw) => {
      crate::style::var_resolution::contains_var(raw)
    }
    _ => false,
  };
  Declaration {
    property: name.into(),
    value,
    contains_var,
    raw_value: String::new(),
    important: false,
  }
}

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

#[test]
fn grid_item_percent_height_resolves_against_definite_grid_area_height() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(100.0));

  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("100px".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", PropertyValue::Keyword("100px".into())),
    &base,
    16.0,
    16.0,
  );

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(10.0));
  let inner = BoxNode::new_block(
    std::sync::Arc::new(inner_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut percent_child_style = ComputedStyle::default();
  percent_child_style.display = Display::Block;
  percent_child_style.height = Some(Length::percent(100.0));
  let percent_child = BoxNode::new_block(
    std::sync::Arc::new(percent_child_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  let grid_item = BoxNode::new_block(
    std::sync::Arc::new(item_style),
    FormattingContextType::Block,
    vec![percent_child],
  );

  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![grid_item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let item_fragment = &fragment.children[0];
  assert_approx(item_fragment.bounds.height(), 100.0, "grid item height");

  assert_eq!(item_fragment.children.len(), 1);
  let percent_fragment = &item_fragment.children[0];
  assert_approx(
    percent_fragment.bounds.height(),
    100.0,
    "percent-height child height",
  );
}
