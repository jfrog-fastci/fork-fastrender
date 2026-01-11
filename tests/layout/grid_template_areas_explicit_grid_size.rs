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

#[test]
fn grid_template_areas_extend_empty_tracks_for_alignment_distribution() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(10.0));
  // Make the block size definite so `align-content: stretch` distributes extra space across rows.
  grid_style.height = Some(Length::px(100.0));

  // One explicit row...
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", PropertyValue::Keyword("auto".into())),
    &base,
    16.0,
    16.0,
  );
  // ...but two rows in the template areas, where the second row is empty (no items).
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", PropertyValue::Keyword("\"a\" \"b\"".into())),
    &base,
    16.0,
    16.0,
  );
  // Center items in their grid area. If the empty `b` row isn't synthesized, the lone `a` row
  // stretches to the full container height and the item will be centered at y=45px.
  apply_declaration(
    &mut grid_style,
    &decl("align-items", PropertyValue::Keyword("center".into())),
    &base,
    16.0,
    16.0,
  );

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut a_style,
    &decl("grid-area", PropertyValue::Keyword("a".into())),
    &base,
    16.0,
    16.0,
  );

  let child_a = BoxNode::new_block(
    std::sync::Arc::new(a_style),
    FormattingContextType::Block,
    vec![],
  );
  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(10.0, 100.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let a = &fragment.children[0];
  // Two rows => 10px content in the first row, 0px in the second, 90px free space. With
  // `align-content: stretch` the free space is split across the two `auto` tracks, so the first
  // row becomes 55px tall and the 10px-tall item is centered at y=22.5px.
  assert_approx(a.bounds.y(), 22.5, "a item y");
}

#[test]
fn grid_template_tracks_can_extend_explicit_grid_past_template_areas() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(40.0));
  grid_style.height = Some(Length::px(10.0));

  // Four explicitly sized tracks...
  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("repeat(4, 10px)".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  // ...but only three columns in the template areas.
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", PropertyValue::Keyword("\"a b c\"".into())),
    &base,
    16.0,
    16.0,
  );

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut a_style,
    &decl("grid-area", PropertyValue::Keyword("a".into())),
    &base,
    16.0,
    16.0,
  );

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  b_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut b_style,
    &decl("grid-area", PropertyValue::Keyword("b".into())),
    &base,
    16.0,
    16.0,
  );

  let mut c_style = ComputedStyle::default();
  c_style.display = Display::Block;
  c_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut c_style,
    &decl("grid-area", PropertyValue::Keyword("c".into())),
    &base,
    16.0,
    16.0,
  );

  let mut auto_style = ComputedStyle::default();
  auto_style.display = Display::Block;
  auto_style.height = Some(Length::px(10.0));

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
  let child_c = BoxNode::new_block(
    std::sync::Arc::new(c_style),
    FormattingContextType::Block,
    vec![],
  );
  let child_auto = BoxNode::new_block(
    std::sync::Arc::new(auto_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a, child_b, child_c, child_auto],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(40.0, 10.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 4);
  let a = &fragment.children[0];
  let b = &fragment.children[1];
  let c = &fragment.children[2];
  let auto = &fragment.children[3];

  assert_approx(a.bounds.x(), 0.0, "a column start");
  assert_approx(a.bounds.width(), 10.0, "a width");
  assert_approx(b.bounds.x(), 10.0, "b column start");
  assert_approx(b.bounds.width(), 10.0, "b width");
  assert_approx(c.bounds.x(), 20.0, "c column start");
  assert_approx(c.bounds.width(), 10.0, "c width");

  // `grid-template-columns` defines four explicit tracks; an auto-placed item should be placed in
  // the remaining fourth cell of the explicit grid, not force implicit columns/rows.
  assert_approx(auto.bounds.x(), 30.0, "auto item column start");
  assert_approx(auto.bounds.y(), 0.0, "auto item row start");
  assert_approx(auto.bounds.width(), 10.0, "auto item width");
}

#[test]
fn grid_template_fr_tracks_can_extend_explicit_grid_past_template_areas() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(400.0));
  grid_style.height = Some(Length::px(10.0));

  // Four flex tracks...
  apply_declaration(
    &mut grid_style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("repeat(4, 1fr)".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  // ...but only three columns in the template areas.
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", PropertyValue::Keyword("\"a b c\"".into())),
    &base,
    16.0,
    16.0,
  );

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut a_style,
    &decl("grid-area", PropertyValue::Keyword("a".into())),
    &base,
    16.0,
    16.0,
  );

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  b_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut b_style,
    &decl("grid-area", PropertyValue::Keyword("b".into())),
    &base,
    16.0,
    16.0,
  );

  let mut c_style = ComputedStyle::default();
  c_style.display = Display::Block;
  c_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut c_style,
    &decl("grid-area", PropertyValue::Keyword("c".into())),
    &base,
    16.0,
    16.0,
  );

  let mut auto_style = ComputedStyle::default();
  auto_style.display = Display::Block;
  auto_style.height = Some(Length::px(10.0));

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
  let child_c = BoxNode::new_block(
    std::sync::Arc::new(c_style),
    FormattingContextType::Block,
    vec![],
  );
  let child_auto = BoxNode::new_block(
    std::sync::Arc::new(auto_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    std::sync::Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a, child_b, child_c, child_auto],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(400.0, 10.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 4);
  let a = &fragment.children[0];
  let b = &fragment.children[1];
  let c = &fragment.children[2];
  let auto = &fragment.children[3];

  assert_approx(a.bounds.x(), 0.0, "a column start");
  assert_approx(a.bounds.width(), 100.0, "a width");
  assert_approx(b.bounds.x(), 100.0, "b column start");
  assert_approx(b.bounds.width(), 100.0, "b width");
  assert_approx(c.bounds.x(), 200.0, "c column start");
  assert_approx(c.bounds.width(), 100.0, "c width");

  // The fourth `fr` track should still exist as part of the explicit grid, and auto-placement
  // should use it rather than creating implicit columns/rows.
  assert_approx(auto.bounds.x(), 300.0, "auto item column start");
  assert_approx(auto.bounds.y(), 0.0, "auto item row start");
  assert_approx(auto.bounds.width(), 100.0, "auto item width");
}
