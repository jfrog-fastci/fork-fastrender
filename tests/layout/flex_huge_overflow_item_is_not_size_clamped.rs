use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContextType, FragmentNode};
use std::sync::Arc;

fn find_fragment_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id().is_some_and(|box_id| box_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_id(child, id) {
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
fn flex_huge_overflow_item_is_not_size_clamped() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_wrap = FlexWrap::NoWrap;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(5000.0));
  child_style.height = Some(Length::px(5000.0));
  child_style.flex_shrink = 0.0;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds");

  let child_fragment = find_fragment_with_id(&fragment, 1).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "huge flex item should not have its x sanitized",
  );
  assert_approx(
    child_fragment.bounds.y(),
    0.0,
    "huge flex item should not have its y sanitized",
  );
  assert_approx(
    child_fragment.bounds.width(),
    5000.0,
    "huge flex item should not have its width clamped",
  );
  assert_approx(
    child_fragment.bounds.height(),
    5000.0,
    "huge flex item should not have its height clamped",
  );
}
