use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::properties::apply_declaration;
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};

fn decl(name: &'static str, value: PropertyValue) -> Declaration {
  let contains_var = match &value {
    PropertyValue::Keyword(raw) | PropertyValue::Custom(raw) => {
      fastrender::style::var_resolution::contains_var(raw)
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
fn grid_template_areas_can_extend_explicit_grid_past_template_tracks() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(30.0));
  grid_style.height = Some(Length::px(10.0));

  // Only one explicitly sized track...
  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("10px".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  // ...but two columns in the template areas.
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", PropertyValue::Keyword("\"a b\"".into())),
    &base,
    16.0,
    16.0,
  );
  // The second column should be created as part of the explicit grid and sized using
  // `grid-auto-columns` per spec.
  apply_declaration(
    &mut grid_style,
    &decl("grid-auto-columns", PropertyValue::Keyword("20px".into())),
    &base,
    16.0,
    16.0,
  );

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  apply_declaration(
    &mut a_style,
    &decl("grid-area", PropertyValue::Keyword("a".into())),
    &base,
    16.0,
    16.0,
  );

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  apply_declaration(
    &mut b_style,
    &decl("grid-area", PropertyValue::Keyword("b".into())),
    &base,
    16.0,
    16.0,
  );

  let child_a = BoxNode::new_block(
    std::sync::Arc::new(a_style),
    FormattingContextType::Block,
    vec![],
  );
  let child_b = BoxNode::new_block(
    std::sync::Arc::new(b_style),
    FormattingContextType::Block,
    vec![],
  );
  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a, child_b],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(30.0, 10.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let a = &fragment.children[0];
  let b = &fragment.children[1];
  assert_approx(a.bounds.x(), 0.0, "a column start");
  assert_approx(a.bounds.width(), 10.0, "a width");
  assert_approx(b.bounds.x(), 10.0, "b column start");
  assert_approx(b.bounds.width(), 20.0, "b width");
}

