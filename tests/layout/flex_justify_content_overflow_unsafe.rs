use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{BoxSizing, FlexDirection, JustifyContent, Overflow};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_child_by_id<'a>(
  fragment: &'a fastrender::tree::fragment_tree::FragmentNode,
  id: usize,
) -> &'a fastrender::tree::fragment_tree::FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == id
      )
    })
    .unwrap_or_else(|| panic!("missing child fragment for box id {id}: {fragment:#?}"))
}

#[test]
fn justify_content_flex_end_with_negative_free_space_respects_padding_floor() {
  // Regression test for Flexbox §9.7(d) "floor the content-box size at zero".
  //
  // Taffy stores flex item main sizes as border-box sizes. If a flex item is allowed to shrink
  // (e.g. `overflow:hidden` => automatic min-size is 0), clamping the border-box size to `0`
  // is incorrect when the item has padding/border: the border-box must still be >= padding+border
  // so the content box is not negative.
  //
  // If the flex algorithm clamps below the padding+border sum, leaf layout later re-inflates the
  // item to satisfy that invariant, and `justify-content` sees inconsistent sizes (so negative
  // free space is miscomputed and items are not end-aligned).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.box_sizing = BoxSizing::BorderBox;
  child_style.width = Some(Length::px(100.0));
  // Shrink a 200px-tall item into a 100px container.
  child_style.height = Some(Length::px(200.0));
  // Add enough padding that the border-box cannot shrink below 150px.
  child_style.padding_top = Length::px(75.0);
  child_style.padding_bottom = Length::px(75.0);
  // Scrollable overflow => automatic minimum size is 0, so the flex algorithm will attempt to
  // shrink below the padding+border sum unless it applies the §9.7(d) floor correctly.
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let child_fragment = find_child_by_id(&fragment, 1);
  let expected_y = fragment.bounds.height() - child_fragment.bounds.height();
  assert!(
    expected_y < -1e-3,
    "test precondition failed: expected negative free space (container_h={}, child_h={})",
    fragment.bounds.height(),
    child_fragment.bounds.height(),
  );
  assert!(
    (child_fragment.bounds.y() - expected_y).abs() < 1e-3,
    "expected flex-end to align the oversized child to the container end (y={expected_y}), got y={} (child_h={}, container_h={})",
    child_fragment.bounds.y(),
    child_fragment.bounds.height(),
    fragment.bounds.height()
  );
}
