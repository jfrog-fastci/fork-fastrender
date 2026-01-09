use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::FontContext;
use std::sync::Arc;

#[test]
fn block_delegation_to_flex_does_not_constrain_auto_height() {
  let viewport = Size::new(200.0, 200.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(150.0));

  let child1 = BoxNode::new_block(Arc::new(child_style.clone()), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let flex_container = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let root = BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![flex_container]);

  // LayoutEngine drives block layout with a definite viewport height; block-level flex containers
  // in normal flow must still size-to-content (auto height) instead of being forced to that
  // definite height.
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(viewport.width),
    AvailableSpace::Definite(viewport.height),
  );
  let fragment = fc.layout(&root, &constraints).expect("layout should succeed");

  let flex_fragment = fragment.children.first().expect("flex fragment");
  assert!(
    (flex_fragment.bounds.height() - 300.0).abs() < 0.5,
    "auto-height flex container should size to content (expected ~300px, got {:.1}px)",
    flex_fragment.bounds.height()
  );
}

