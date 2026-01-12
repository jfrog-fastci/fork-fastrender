use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn percent_width_block(percent: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::percent(percent));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Prevent the line breaking behavior from being obscured by flex shrinking.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

fn find_child<'a>(
  fragment: &'a fastrender::FragmentNode,
  box_id: usize,
) -> &'a fastrender::FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
}

#[test]
fn flex_wrap_percent_width_items_wrap_to_new_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut first = BoxNode::new_block(
    percent_width_block(100.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  first.id = 2;

  let mut second = BoxNode::new_block(
    percent_width_block(100.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  second.id = 3;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![first, second],
  );
  container.id = 1;

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let first_fragment = find_child(&fragment, 2);
  let second_fragment = find_child(&fragment, 3);

  assert!(
    (first_fragment.bounds.width() - 100.0).abs() < 0.1,
    "expected first child width≈100px, got {:.2}",
    first_fragment.bounds.width()
  );
  assert!(
    first_fragment.bounds.x().abs() < 0.1,
    "expected first child x≈0px, got {:.2}",
    first_fragment.bounds.x()
  );
  assert!(
    first_fragment.bounds.y().abs() < 0.1,
    "expected first child y≈0px, got {:.2}",
    first_fragment.bounds.y()
  );

  assert!(
    (second_fragment.bounds.width() - 100.0).abs() < 0.1,
    "expected second child width≈100px, got {:.2}",
    second_fragment.bounds.width()
  );
  assert!(
    second_fragment.bounds.x().abs() < 0.1,
    "expected second child to wrap to the start edge (x≈0px), got {:.2}",
    second_fragment.bounds.x()
  );
  assert!(
    second_fragment.bounds.y() >= 10.0 - 0.6,
    "expected second child to wrap to the next line (y>=10px), got {:.2}",
    second_fragment.bounds.y()
  );
}
