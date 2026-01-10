use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
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
fn grid_auto_height_sizes_to_content_in_scrollable_layout() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;

  // Two-column grid like the BBC hero section; no explicit height/min-height.
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", PropertyValue::Keyword("1fr 1fr".into())),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", PropertyValue::Keyword("auto".into())),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("justify-content", PropertyValue::Keyword("space-between".into())),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("align-items", PropertyValue::Keyword("center".into())),
    &base,
    16.0,
    16.0,
  );

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.height = Some(Length::px(200.0));

  let mut short_style = ComputedStyle::default();
  short_style.display = Display::Block;
  short_style.height = Some(Length::px(100.0));

  let child_tall = BoxNode::new_block(
    std::sync::Arc::new(tall_style),
    FormattingContextType::Block,
    vec![],
  );
  let child_short = BoxNode::new_block(
    std::sync::Arc::new(short_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_tall, child_short],
  );

  // The default GridFormattingContext viewport height is 600px. When the available height is
  // indefinite (normal block flow), the grid container must still size-to-content rather than
  // filling that viewport height.
  let fc = GridFormattingContext::new();
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(400.0), AvailableSpace::Indefinite);
  let fragment = fc.layout(&grid, &constraints).expect("layout succeeds");

  assert_approx(
    fragment.bounds.height(),
    200.0,
    "grid container auto height should size to tallest row content",
  );

  let min_child_y = fragment
    .children
    .iter()
    .map(|child| child.bounds.y())
    .fold(f32::INFINITY, f32::min);
  assert_approx(min_child_y, 0.0, "grid items should not be vertically offset");
}

