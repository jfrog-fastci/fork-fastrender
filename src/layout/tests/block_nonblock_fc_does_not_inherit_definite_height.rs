use crate::geometry::Size;
use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::FlexDirection;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::FontContext;
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

  let child1 = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let flex_container = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![flex_container],
  );

  // LayoutEngine drives block layout with a definite viewport height; block-level flex containers
  // in normal flow must still size-to-content (auto height) instead of being forced to that
  // definite height.
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(viewport.width),
    AvailableSpace::Definite(viewport.height),
  );
  let fragment = fc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let flex_fragment = fragment.children.first().expect("flex fragment");
  assert!(
    (flex_fragment.bounds.height() - 300.0).abs() < 0.5,
    "auto-height flex container should size to content (expected ~300px, got {:.1}px)",
    flex_fragment.bounds.height()
  );
}

#[test]
fn block_delegation_to_flex_does_not_constrain_auto_height_with_definite_parent_height() {
  let viewport = Size::new(200.0, 400.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.height = Some(Length::px(200.0));

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(150.0));

  let child1 = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let flex_container = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![flex_container],
  );

  // A definite containing-block height provides a percentage basis, but must not force normal-flow
  // flex containers with `height:auto` to fill the available height.
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(viewport.width),
    AvailableSpace::Definite(viewport.height),
  );
  let fragment = fc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 200.0).abs() < 0.5,
    "root fragment should honor specified height (expected ~200px, got {:.1}px)",
    fragment.bounds.height()
  );

  let flex_fragment = fragment.children.first().expect("flex fragment");
  assert!(
    (flex_fragment.bounds.height() - 300.0).abs() < 0.5,
    "auto-height flex container should size to content (expected ~300px, got {:.1}px)",
    flex_fragment.bounds.height()
  );
}
