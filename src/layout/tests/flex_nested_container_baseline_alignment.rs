use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::position::Position;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn first_baseline_from_content(fragment: &FragmentNode) -> Option<f32> {
  if let Some(style) = fragment.style.as_deref() {
    if style.running_position.is_some()
      || matches!(style.position, Position::Absolute | Position::Fixed)
    {
      return None;
    }
  }

  match &fragment.content {
    FragmentContent::Line { baseline } => Some(*baseline),
    FragmentContent::Text {
      baseline_offset, ..
    } => Some(*baseline_offset),
    FragmentContent::Replaced { .. } => Some(fragment.bounds.height()),
    _ => {
      for child in fragment.children.iter() {
        if let Some(b) = first_baseline_from_content(child) {
          return Some(child.bounds.y() + b);
        }
      }
      None
    }
  }
}

#[test]
fn flex_nested_container_baseline_alignment() {
  let fc = FlexFormattingContext::new();

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Row;
  outer_style.align_items = AlignItems::Baseline;
  outer_style.width = Some(Length::px(240.0));

  // Child A: simple text baseline near the top of its box.
  let mut child_a_text_style = ComputedStyle::default();
  child_a_text_style.display = Display::Inline;
  child_a_text_style.font_size = 20.0;
  let child_a_text = BoxNode::new_text(Arc::new(child_a_text_style), "A".to_string());

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.width = Some(Length::px(40.0));
  let child_a = BoxNode::new_block(
    Arc::new(child_a_style),
    FormattingContextType::Block,
    vec![child_a_text],
  );

  // Child B: nested flex container with a definite height larger than its text, and `align-items:
  // center` so its internal flex item gets a non-zero cross-axis offset.
  let mut nested_style = ComputedStyle::default();
  nested_style.display = Display::Flex;
  nested_style.flex_direction = FlexDirection::Row;
  nested_style.align_items = AlignItems::Center;
  nested_style.width = Some(Length::px(100.0));
  nested_style.height = Some(Length::px(100.0));

  let mut nested_item_style = ComputedStyle::default();
  nested_item_style.display = Display::Block;
  nested_item_style.width = Some(Length::px(40.0));

  let mut nested_text_style = ComputedStyle::default();
  nested_text_style.display = Display::Inline;
  nested_text_style.font_size = 20.0;
  let nested_text = BoxNode::new_text(Arc::new(nested_text_style), "B".to_string());

  let nested_item = BoxNode::new_block(
    Arc::new(nested_item_style),
    FormattingContextType::Block,
    vec![nested_text],
  );

  let nested = BoxNode::new_block(
    Arc::new(nested_style),
    FormattingContextType::Flex,
    vec![nested_item],
  );

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![child_a, nested],
  );

  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite_width(240.0))
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let a = &fragment.children[0];
  let b = &fragment.children[1];

  let baseline_a = first_baseline_from_content(a).expect("child A should have a baseline");
  let baseline_b = first_baseline_from_content(b).expect("child B should have a baseline");

  let baseline_pos_a = a.bounds.y() + baseline_a;
  let baseline_pos_b = b.bounds.y() + baseline_b;
  let eps = 0.75;
  assert!(
    (baseline_pos_a - baseline_pos_b).abs() < eps,
    "expected nested flex container to participate in baseline alignment (a={baseline_pos_a:.2}, b={baseline_pos_b:.2})"
  );

  // Ensure the nested flex item actually received a non-zero cross-axis alignment offset (i.e. it
  // was centered within the taller nested container).
  assert_eq!(b.children.len(), 1);
  let nested_item_fragment = &b.children[0];
  assert!(
    nested_item_fragment.bounds.y() > 1.0,
    "expected nested flex item to be vertically centered (offset_y={:.2})",
    nested_item_fragment.bounds.y()
  );
}
