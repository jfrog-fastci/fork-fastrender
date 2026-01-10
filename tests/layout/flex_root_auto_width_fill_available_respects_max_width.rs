use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::FragmentNode;
use std::sync::Arc;

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

#[test]
fn flex_root_auto_width_fill_available_respects_max_width() {
  let fc = FlexFormattingContext::with_viewport(Size::new(800.0, 600.0));

  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.justify_content = JustifyContent::FlexEnd;
  style.max_width = Some(Length::px(150.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(20.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    Vec::new(),
  );
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(300.0))
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.width() - 150.0).abs() < 0.5,
    "expected max-width to clamp fill-available width:auto (got {:.1})",
    fragment.bounds.width()
  );

  // Ensure we don't correct the root size without rerunning flex layout, which would cause children
  // to be positioned as though the container were still 300px wide.
  let child_fragment = fragment
    .children
    .iter()
    .find(|f| fragment_box_id(f) == Some(1))
    .expect("expected flex child with id=1");
  assert!(
    (child_fragment.bounds.x() - 130.0).abs() < 0.5,
    "expected justify-content:flex-end to place child at x=130px (got {:.1}, root_w={:.1})",
    child_fragment.bounds.x(),
    fragment.bounds.width()
  );
  assert!(
    child_fragment.bounds.max_x() <= fragment.bounds.width() + 0.5,
    "expected child to remain within the clamped root width (child_bounds={:?}, root_bounds={:?})",
    child_fragment.bounds,
    fragment.bounds
  );
}
