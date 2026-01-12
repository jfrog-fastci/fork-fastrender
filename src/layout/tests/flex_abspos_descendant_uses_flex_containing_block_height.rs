use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{FlexDirection, InsetValue};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
) -> Option<&'a FragmentNode> {
  let matches = match fragment.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline {
      box_id: Some(id), ..
    }
    | FragmentContent::Text {
      box_id: Some(id), ..
    }
    | FragmentContent::Replaced {
      box_id: Some(id), ..
    } => id == box_id,
    _ => false,
  };
  if matches {
    return Some(fragment);
  }
  for child in &fragment.children {
    if let Some(found) = find_fragment_by_box_id(child, box_id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn flex_abspos_descendant_uses_flex_containing_block_height() {
  // Regression: flex layout performs a measure pass before the container's final used block-size is
  // known. Nested formatting contexts can therefore lay out abspos descendants against an unrelated
  // ancestor containing block (often the viewport), and the measured fragment may be reused without
  // re-layout once the flex container establishes its own containing block.
  //
  // The abspos child below is a *descendant* (not a direct child) of the flex container, but its
  // nearest positioned ancestor is the flex container. `top:0; bottom:0` must therefore stretch
  // the element to the flex container's used padding-box height (100px), not the viewport height
  // (200px).
  let fc = FlexFormattingContext::with_viewport(Size::new(100.0, 200.0));

  let abs_id = 2usize;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = abs_id;

  let wrapper = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![abs_child],
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.height = Some(Length::px(80.0));
  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![wrapper],
  );

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.padding_top = Length::px(10.0);
  container_style.padding_bottom = Length::px(10.0);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(100.0),
        AvailableSpace::Definite(200.0),
      ),
    )
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.height() - 100.0).abs() < 0.1,
    "flex container should shrink-wrap to in-flow content + padding (expected 100px, got {})",
    fragment.bounds.height()
  );

  let abs_fragment = find_fragment_by_box_id(&fragment, abs_id).expect("abspos fragment");
  assert!(
    (abs_fragment.bounds.height() - 100.0).abs() < 0.1,
    "abspos descendant should size against the flex container's used CB height (expected 100px, got {})",
    abs_fragment.bounds.height()
  );
}
