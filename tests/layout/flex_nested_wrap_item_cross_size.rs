use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, FlexDirection, FlexWrap};
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

#[test]
fn nested_wrapping_flex_item_affects_parent_cross_size() {
  let fc = FlexFormattingContext::new();

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Flex;
  parent_style.flex_direction = FlexDirection::Row;
  parent_style.flex_wrap = FlexWrap::NoWrap;
  parent_style.align_items = AlignItems::Center;
  parent_style.width = Some(Length::px(200.0));
  let parent_style = Arc::new(parent_style);

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::px(30.0));
  left_style.height = Some(Length::px(20.0));
  left_style.flex_grow = 0.0;
  left_style.flex_shrink = 0.0;
  let mut left = BoxNode::new_block(Arc::new(left_style), FormattingContextType::Block, vec![]);
  left.id = 1;

  let mut wrap_style = ComputedStyle::default();
  wrap_style.display = Display::Flex;
  wrap_style.flex_direction = FlexDirection::Row;
  wrap_style.flex_wrap = FlexWrap::Wrap;
  wrap_style.grid_column_gap = Length::px(10.0);
  wrap_style.grid_row_gap = Length::px(5.0);
  // Allow the wrapper itself to shrink so its used width is the remaining space in the parent.
  wrap_style.flex_grow = 0.0;
  wrap_style.flex_shrink = 1.0;

  fn fixed_item(id: usize) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::px(80.0));
    style.height = Some(Length::px(20.0));
    // Keep flex items from shrinking so wrapping is driven by the container width.
    style.flex_grow = 0.0;
    style.flex_shrink = 0.0;
    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    node.id = id;
    node
  }

  // Max-content width would be 80 + 10 + 80 + 10 + 80 = 260. When the parent clamps the used
  // width to 200 - 30 = 170, the third item wraps onto a second line. The wrapper's auto height
  // should include both lines plus the row gap:
  // 20 (line 1) + 5 (row gap) + 20 (line 2) = 45.
  let mut wrapper = BoxNode::new_block(
    Arc::new(wrap_style),
    FormattingContextType::Flex,
    vec![fixed_item(2), fixed_item(3), fixed_item(4)],
  );
  wrapper.id = 10;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Flex,
    vec![left, wrapper],
  );
  parent.id = 20;

  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let wrapper_fragment = find_fragment_with_id(&fragment, 10).expect("wrapper fragment");
  let eps = 1e-3;
  assert!(
    (wrapper_fragment.bounds.height() - 45.0).abs() < eps,
    "expected nested wrapping flex item height to reflect wrapped lines, got {}",
    wrapper_fragment.bounds.height()
  );
  assert!(
    (fragment.bounds.height() - 45.0).abs() < eps,
    "expected parent flex container to include wrapped child height in its cross size, got {}",
    fragment.bounds.height()
  );
}
