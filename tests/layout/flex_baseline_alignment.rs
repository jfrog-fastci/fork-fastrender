use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::AlignItems;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn first_baseline_offset(fragment: &FragmentNode) -> Option<f32> {
  if let Some(style) = fragment.style.as_deref() {
    if style.running_position.is_some() || matches!(style.position, Position::Absolute | Position::Fixed) {
      return None;
    }
  }

  if let Some(baseline) = fragment.baseline {
    return Some(baseline);
  }

  match &fragment.content {
    FragmentContent::Line { baseline } => Some(*baseline),
    FragmentContent::Text { baseline_offset, .. } => Some(*baseline_offset),
    FragmentContent::Replaced { .. } => Some(fragment.bounds.height()),
    _ => {
      for child in fragment.children.iter() {
        if let Some(b) = first_baseline_offset(child) {
          return Some(child.bounds.y() + b);
        }
      }
      None
    }
  }
}

#[test]
fn flex_align_items_baseline_influences_cross_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  container_style.align_items = AlignItems::Baseline;

  // Child A has no in-flow baseline (empty block). Its baseline should fall back to the bottom
  // edge of its margin box (i.e., its height).
  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.width = Some(Length::px(10.0));
  child_a_style.height = Some(Length::px(100.0));
  let child_a = BoxNode::new_block(Arc::new(child_a_style), FormattingContextType::Block, vec![]);

  // Child B has a baseline near the top (text at y=0) but a large fixed height, so baseline
  // alignment should push it down and increase the line's cross size beyond 100px.
  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.font_size = 20.0;
  let text = BoxNode::new_text(Arc::new(text_style), "Hello".to_string());

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.width = Some(Length::px(10.0));
  child_b_style.height = Some(Length::px(100.0));
  let child_b =
    BoxNode::new_block(Arc::new(child_b_style), FormattingContextType::Block, vec![text]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let child_a_frag = &fragment.children[0];
  let child_b_frag = &fragment.children[1];

  let baseline_a = first_baseline_offset(child_a_frag).unwrap_or(child_a_frag.bounds.height());
  let baseline_b = first_baseline_offset(child_b_frag).unwrap_or(child_b_frag.bounds.height());
  let max_baseline = baseline_a.max(baseline_b);

  let expected_cross = [
    max_baseline - baseline_a + child_a_frag.bounds.height(),
    max_baseline - baseline_b + child_b_frag.bounds.height(),
  ]
  .into_iter()
  .fold(0.0, f32::max);

  let container_cross = fragment.bounds.height();
  let eps = 0.75;
  assert!(
    (container_cross - expected_cross).abs() < eps,
    "expected baseline alignment to increase the flex line cross size (expected≈{expected_cross:.2}, got {container_cross:.2})"
  );

  let baseline_pos_a = child_a_frag.bounds.y() + baseline_a;
  let baseline_pos_b = child_b_frag.bounds.y() + baseline_b;
  assert!(
    (baseline_pos_a - baseline_pos_b).abs() < eps,
    "baselines should align (child_a={baseline_pos_a:.2}, child_b={baseline_pos_b:.2})"
  );
}

