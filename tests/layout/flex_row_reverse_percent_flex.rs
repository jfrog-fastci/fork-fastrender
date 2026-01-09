use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext};
use std::sync::Arc;

fn find_child_by_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  fragment.children.iter().find(|child| {
    matches!(
      child.content,
      FragmentContent::Block { box_id: Some(box_id) }
        | FragmentContent::Inline { box_id: Some(box_id), .. }
        | FragmentContent::Text { box_id: Some(box_id), .. }
        | FragmentContent::Replaced { box_id: Some(box_id), .. }
        if box_id == id
    )
  })
}

#[test]
fn flex_row_reverse_percent_sized_item_keeps_flex_grow_sibling_in_flow() {
  // Regression test for `flex-direction: row-reverse` with a percentage-sized item and a
  // `flex: 1` sibling.
  //
  // On abcnews.go.com this manifests as the "headline list" column being laid out off to the right
  // (starting at x=100%) instead of occupying the remaining space on the left.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.width = Some(Length::px(621.0));
  container_style.height = Some(Length::px(100.0));

  let mut fixed_style = ComputedStyle::default();
  fixed_style.display = Display::Block;
  fixed_style.width = Some(Length::percent(60.0));
  fixed_style.height = Some(Length::px(100.0));
  fixed_style.flex_shrink = 0.0;
  let mut fixed =
    BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);
  fixed.id = 1;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_grow = 1.0;
  flex_style.flex_shrink = 1.0;
  flex_style.flex_basis = FlexBasis::Length(Length::percent(0.0));
  flex_style.height = Some(Length::px(100.0));
  let mut flex_child =
    BoxNode::new_block(Arc::new(flex_style), FormattingContextType::Flex, vec![]);
  flex_child.id = 2;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed, flex_child],
  );
  container.id = 100;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(621.0, 100.0))
    .expect("layout succeeds");

  let fixed_frag = find_child_by_id(&fragment, 1).expect("fixed fragment");
  let flex_frag = find_child_by_id(&fragment, 2).expect("flex fragment");

  // Fixed item should be positioned at the right edge (row-reverse).
  let fixed_w = fixed_frag.bounds.width();
  assert!(
    (fixed_frag.bounds.x() - (621.0 - fixed_w)).abs() < 0.5,
    "expected fixed child to be placed at right edge; got x={}, w={}",
    fixed_frag.bounds.x(),
    fixed_w
  );

  // The flex:1 item should occupy the remaining space on the left, starting at x=0.
  assert!(
    flex_frag.bounds.x().abs() < 0.5,
    "expected flex-grow child to start at x=0; got x={} (w={})",
    flex_frag.bounds.x(),
    flex_frag.bounds.width()
  );
}

