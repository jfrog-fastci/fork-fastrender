use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_max_content_height_is_not_clamped_to_viewport() {
  let viewport = Size::new(200.0, 200.0);
  let fc = FlexFormattingContext::with_viewport(viewport);

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Column;

  let mut header_style = ComputedStyle::default();
  header_style.display = Display::Block;
  header_style.height = Some(Length::px(10.0));
  let header = BoxNode::new_block(Arc::new(header_style), FormattingContextType::Block, vec![]);

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Flex;
  inner_style.flex_direction = FlexDirection::Column;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.height = Some(Length::px(300.0));
  let tall_child = BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);

  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Flex,
    vec![tall_child],
  );

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![header, inner],
  );

  // When the flex container's main size is auto (indefinite height), Taffy asks for max-content
  // measurements. The flex item should be allowed to measure taller than the viewport.
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(viewport.width), AvailableSpace::Indefinite);
  let fragment = fc.layout(&outer, &constraints).expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 310.0).abs() < 0.5,
    "outer auto-height flex container should include tall child (expected ~310px, got {:.1}px)",
    fragment.bounds.height()
  );

  let inner_fragment = fragment.children.get(1).expect("inner fragment");
  assert!(
    (inner_fragment.bounds.height() - 300.0).abs() < 0.5,
    "inner auto-height flex container should size to tall child (expected ~300px, got {:.1}px)",
    inner_fragment.bounds.height()
  );
}

