use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{AlignItems, FlexDirection};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn line_count(fragment: &FragmentNode) -> usize {
  fragment
    .iter_fragments()
    .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    .count()
}

fn descendant_max_right(fragment: &FragmentNode) -> f32 {
  let origin_x = fragment.bounds.x();
  fragment
    .iter_fragments()
    .map(|f| f.bounds.max_x() - origin_x)
    .fold(0.0, f32::max)
}

#[test]
fn flex_constraints_do_not_clamp_definite_width_to_viewport() {
  let fc = FlexFormattingContext::with_viewport(Size::new(200.0, 200.0));

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  // With a column flex container, `align-items: stretch` should give the child a definite inline
  // size equal to the container width.
  container_style.align_items = AlignItems::Stretch;
  container_style.width = Some(Length::px(1000.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;

  let text = "aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa";
  let text_node = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Inline,
    vec![text_node],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(1000.0), AvailableSpace::Indefinite);
  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.width() - 1000.0).abs() < 0.5,
    "flex container width should not be clamped to the viewport (got {:.1})",
    fragment.bounds.width()
  );

  let child_fragment = fragment.children.first().expect("child fragment");
  let lines = line_count(child_fragment);
  let max_right = descendant_max_right(child_fragment);
  assert_eq!(
    lines, 1,
    "child should not wrap when flex has a definite width larger than the viewport (child_width={:.1}, max_right={:.1})",
    child_fragment.bounds.width(),
    max_right
  );
}
