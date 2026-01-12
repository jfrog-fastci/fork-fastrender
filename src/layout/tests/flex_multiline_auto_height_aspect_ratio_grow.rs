use std::sync::Arc;

use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::{AlignItems, AspectRatio, FlexDirection, FlexWrap};
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};

#[test]
fn flex_multiline_auto_height_includes_lines_after_flex_grow_with_aspect_ratio() {
  // Items whose cross size depends on main-size flexing (via aspect-ratio) must still contribute
  // to the flex container's auto height across multiple lines.
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.grid_row_gap = Length::px(5.0);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(80.0));
  item_style.width_keyword = None;
  item_style.flex_grow = 1.0;
  item_style.flex_shrink = 1.0;
  item_style.aspect_ratio = AspectRatio::Ratio(1.0);
  // Height is `auto`, so aspect-ratio determines it from the flexed width.
  let item_style = Arc::new(item_style);

  let mut item1 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
  item1.id = 1;
  let mut item2 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
  item2.id = 2;
  let mut item3 = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
  item3.id = 3;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item1, item2, item3],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let mut max_child_bottom = 0.0f32;
  let mut child_debug = Vec::new();
  for child in fragment.children.iter() {
    max_child_bottom = max_child_bottom.max(child.bounds.max_y());
    child_debug.push((
      child.box_id(),
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
  }

  // Line 1 contains two 80px items -> they flex-grow to 100px each -> height 100.
  // Line 2 contains one item -> it flex-grows to 200px -> height 200.
  // Row gap: 5px.
  let expected_height = 100.0 + 5.0 + 200.0;
  let eps = 1e-3;
  assert!(
    (fragment.bounds.height() - expected_height).abs() < eps,
    "expected flex container auto height to include all lines after flexing, got {} (max_child_bottom={} children={:?})",
    fragment.bounds.height(),
    max_child_bottom,
    child_debug
  );
}
