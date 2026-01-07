use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use fastrender::FragmentNode;
use std::sync::Arc;

fn find_first_fragment_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id().is_some_and(|fragment_id| fragment_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn assert_approx(value: f32, expected: f32, msg: &str) {
  assert!(
    (value - expected).abs() <= 0.5,
    "{msg}: got {value:.2} expected {expected:.2}"
  );
}

#[test]
fn flex_basis_content_ignores_preferred_main_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  let container_style = Arc::new(container_style);

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.width = Some(Length::px(100.0));
  child_a_style.flex_grow = 0.0;
  child_a_style.flex_shrink = 0.0;
  child_a_style.flex_basis = FlexBasis::Content;
  let child_a_style = Arc::new(child_a_style);

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.width = Some(Length::px(50.0));
  child_b_style.flex_grow = 0.0;
  child_b_style.flex_shrink = 0.0;
  child_b_style.flex_basis = FlexBasis::Auto;
  let child_b_style = Arc::new(child_b_style);

  let mut child_a = BoxNode::new_block(child_a_style, FormattingContextType::Block, vec![]);
  child_a.id = 1;
  let mut child_b = BoxNode::new_block(child_b_style, FormattingContextType::Block, vec![]);
  child_b.id = 2;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );
  container.id = 3;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  let child_a_fragment = find_first_fragment_with_id(&fragment, 1).expect("child A fragment");
  let child_b_fragment = find_first_fragment_with_id(&fragment, 2).expect("child B fragment");

  assert_approx(child_a_fragment.bounds.width(), 0.0, "child A should size to its content");
  assert_approx(
    child_b_fragment.bounds.width(),
    50.0,
    "child B should size using its preferred width",
  );
  assert_approx(
    child_b_fragment.bounds.x(),
    0.0,
    "child B should be positioned at the container start",
  );
}

#[test]
fn flex_basis_content_ignores_preferred_main_size_in_column_flex() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.height = Some(Length::px(200.0));
  let container_style = Arc::new(container_style);

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.height = Some(Length::px(100.0));
  child_a_style.flex_grow = 0.0;
  child_a_style.flex_shrink = 0.0;
  child_a_style.flex_basis = FlexBasis::Content;
  let child_a_style = Arc::new(child_a_style);

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.height = Some(Length::px(50.0));
  child_b_style.flex_grow = 0.0;
  child_b_style.flex_shrink = 0.0;
  child_b_style.flex_basis = FlexBasis::Auto;
  let child_b_style = Arc::new(child_b_style);

  let mut child_a = BoxNode::new_block(child_a_style, FormattingContextType::Block, vec![]);
  child_a.id = 10;
  let mut child_b = BoxNode::new_block(child_b_style, FormattingContextType::Block, vec![]);
  child_b.id = 11;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );
  container.id = 12;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let child_a_fragment = find_first_fragment_with_id(&fragment, 10).expect("child A fragment");
  let child_b_fragment = find_first_fragment_with_id(&fragment, 11).expect("child B fragment");

  assert_approx(
    child_a_fragment.bounds.height(),
    0.0,
    "child A should size to its content on the main axis",
  );
  assert_approx(
    child_b_fragment.bounds.height(),
    50.0,
    "child B should size using its preferred height",
  );
  assert_approx(
    child_b_fragment.bounds.y(),
    0.0,
    "child B should be positioned at the container start",
  );
}
