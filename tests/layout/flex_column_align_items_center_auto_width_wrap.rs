use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, FlexDirection};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
}

fn count_line_fragments(fragment: &FragmentNode) -> usize {
  fn walk(node: &FragmentNode, count: &mut usize) {
    if matches!(node.content, FragmentContent::Line { .. }) {
      *count += 1;
    }
    for child in node.children.iter() {
      walk(child, count);
    }
  }

  let mut count = 0usize;
  walk(fragment, &mut count);
  count
}

#[test]
fn flex_column_align_items_center_auto_width_wraps_and_clamps_to_available_inline_size() {
  // Regression test for column flex containers with `align-items:center`:
  // - Flex items with `width:auto` are not stretched in the cross axis.
  // - The used cross size is the `fit-content` size clamped against the available width.
  // - Text should wrap when the clamped width is smaller than the max-content width.
  //
  // Previously the flex measure callback treated a definite available cross size as max-content,
  // preventing wrapping and producing an over-wide flex item (observed on reddit.com).
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.align_items = AlignItems::Center;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;

  let mut text_style = ComputedStyle::default();
  text_style.font_size = 16.0;
  let text_style = Arc::new(text_style);

  let text = BoxNode::new_text(
    text_style.clone(),
    "This is a long sentence that should wrap onto multiple lines when constrained.".to_string(),
  );
  let inline = BoxNode::new_inline(text_style, vec![text]);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = None;
  item_style.width_keyword = None;
  let mut item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![inline]);
  item.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");
  let item_fragment = find_child(&fragment, 2);

  assert!(
    item_fragment.bounds.width() <= 200.0 + 0.5,
    "expected item width to be clamped to the available width (got {:.2})",
    item_fragment.bounds.width()
  );

  let line_count = count_line_fragments(item_fragment);
  assert!(
    line_count >= 2,
    "expected wrapped text to produce multiple line fragments, got {line_count}"
  );
}
