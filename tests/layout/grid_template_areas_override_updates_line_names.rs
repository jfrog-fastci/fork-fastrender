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
fn grid_template_areas_override_updates_synthesized_area_line_names() {
  // Britannica's fixture uses `grid-template-areas` in a base rule and then overrides it in a
  // media query without updating `grid-template-rows`. The grid items are positioned via
  // `grid-area: <name>`, which resolves using implicit `<name>-start` / `<name>-end` line names.
  //
  // When we fail to clear synthesized line names across overrides, the item can keep using the
  // *old* row line indices (typically placing the area at the top and spanning multiple rows),
  // causing massive overlap.
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(30.0));
  grid_style.height = Some(Length::px(30.0));

  // Base template: 3 columns, `b` spans all rows in the third column.
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", PropertyValue::Keyword("10px 10px 10px".into())),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", PropertyValue::Keyword("10px 10px 10px".into())),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-areas",
      PropertyValue::Keyword("\"a a b\" \"c c b\" \"c c b\"".into()),
    ),
    &base,
    16.0,
    16.0,
  );

  // Override template: 2 columns, `b` moves to the last row spanning both columns.
  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-areas",
      PropertyValue::Keyword("\"a a\" \"c c\" \"b b\"".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", PropertyValue::Keyword("10px 10px".into())),
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

  let mut c_style = ComputedStyle::default();
  c_style.display = Display::Block;
  apply_declaration(
    &mut c_style,
    &decl("grid-area", PropertyValue::Keyword("c".into())),
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

  let child_a = BoxNode::new_block(std::sync::Arc::new(a_style), FormattingContextType::Block, vec![]);
  let child_c = BoxNode::new_block(std::sync::Arc::new(c_style), FormattingContextType::Block, vec![]);
  let child_b = BoxNode::new_block(std::sync::Arc::new(b_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a, child_c, child_b],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(30.0, 30.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 3);
  let a = &fragment.children[0];
  let c = &fragment.children[1];
  let b = &fragment.children[2];

  assert_approx(a.bounds.x(), 0.0, "a x");
  assert_approx(a.bounds.y(), 0.0, "a y");
  assert_approx(a.bounds.width(), 20.0, "a width");
  assert_approx(a.bounds.height(), 10.0, "a height");

  assert_approx(c.bounds.x(), 0.0, "c x");
  assert_approx(c.bounds.y(), 10.0, "c y");
  assert_approx(c.bounds.width(), 20.0, "c width");
  assert_approx(c.bounds.height(), 10.0, "c height");

  assert_approx(b.bounds.x(), 0.0, "b x");
  assert_approx(b.bounds.y(), 20.0, "b y");
  assert_approx(b.bounds.width(), 20.0, "b width");
  assert_approx(b.bounds.height(), 10.0, "b height");
}
