use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{AlignItems, BoxSizing, FlexDirection};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{ComputedStyle, FragmentNode};

fn fragment_box_id(fragment: &FragmentNode) -> Option<usize> {
  match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

fn fixed_block(width: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  style.flex_grow = 0.0;
  style.flex_shrink = 0.0;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, Vec::new())
}

#[test]
fn flex_column_align_items_center_auto_width_honors_max_width() {
  // reddit.com's offline fixture centers a max-width text column inside a padded full-width flex
  // container. If the flex item incorrectly stretches, the content (and text wrapping) will be
  // wrong.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.align_items = AlignItems::Center;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.padding_left = Length::px(16.0);
  container_style.padding_right = Length::px(16.0);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.max_width = Some(Length::px(480.0));
  item_style.max_width_keyword = None;

  // Give the item a large intrinsic width so `max-width` must clamp it.
  let mut item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![fixed_block(800.0)],
  );
  item.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(1040.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  let item_fragment = fragment
    .children
    .iter()
    .find(|child| fragment_box_id(child) == Some(1))
    .expect("expected flex item fragment");

  assert!(
    (item_fragment.bounds.width() - 480.0).abs() < 0.5,
    "expected max-width to clamp item width to 480px (got {:.1})",
    item_fragment.bounds.width()
  );

  // Container inner width is 1040 - 16px - 16px = 1008px, so the centered item should start at:
  // padding_left + (1008 - 480)/2 = 280px.
  assert!(
    (item_fragment.bounds.x() - 280.0).abs() < 0.5,
    "expected align-items:center to center the item at x=280px (got {:.1})",
    item_fragment.bounds.x()
  );
}

