use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::Overflow;
use fastrender::style::types::WhiteSpace;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

static NEXT_ID: AtomicUsize = AtomicUsize::new(1_000_000);

fn make_flex_container_with_overflow(overflow_x: Overflow) -> BoxNode {
  let next_id = || NEXT_ID.fetch_add(1, Ordering::Relaxed);

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.overflow_x = overflow_x;
  child_style.overflow_y = Overflow::Clip;

  let mut text_style = ComputedStyle::default();
  text_style.white_space = WhiteSpace::Nowrap;
  // Keep the word short enough to stay below the overflow "runaway" sanitization threshold
  // (currently 20× the container width) so we can observe the actual flex item size.
  let long_word = "W".repeat(50);
  let mut text = BoxNode::new_text(Arc::new(text_style), long_word);
  text.id = next_id();

  // Wrap the text in a block box so `FlexFormattingContext::compute_intrinsic_inline_size`
  // can compute a non-zero min-content width: text nodes do not establish a formatting context
  // and would otherwise be treated as block formatting context leaves (which yields 0).
  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.white_space = WhiteSpace::Nowrap;
  let mut inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Block,
    vec![text],
  );
  inner.id = next_id();

  let mut child_box = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![inner],
  );
  child_box.id = next_id();

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_box],
  );
  container.id = next_id();

  container
}

#[test]
fn flex_auto_min_size_applies_for_overflow_clip() {
  let container = make_flex_container_with_overflow(Overflow::Clip);
  let fc = FlexFormattingContext::new();
  let child = &container.children[0];
  let min_content = fc
    .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MinContent)
    .expect("min-content size");
  assert!(
    min_content > 50.5,
    "test precondition failed: expected min-content width > container width (got {min_content})"
  );
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(50.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");
  assert_eq!(fragment.children.len(), 1);
  let child_width = fragment.children[0].bounds.width();
  let eps = 1.0;
  assert!(
    (child_width + eps) >= min_content,
    "overflow: clip should be treated as non-scrollable so min-width:auto uses min-content width; expected >= {min_content}, got {child_width}"
  );
}

#[test]
fn flex_auto_min_size_is_zero_for_scrollable_overflow() {
  let clip_container = make_flex_container_with_overflow(Overflow::Clip);
  let clip_child = &clip_container.children[0];
  let fc = FlexFormattingContext::new();
  let min_content = fc
    .compute_intrinsic_inline_size(clip_child, IntrinsicSizingMode::MinContent)
    .expect("min-content size");
  assert!(
    min_content > 50.5,
    "test precondition failed: expected min-content width > container width (got {min_content})"
  );

  let container = make_flex_container_with_overflow(Overflow::Hidden);
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(50.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");
  assert_eq!(fragment.children.len(), 1);
  let child_width = fragment.children[0].bounds.width();
  let eps = 0.5;
  assert!(
    child_width <= 50.0 + eps,
    "overflow: hidden should make the flex item a scroll container so it can shrink to the container width; expected <= 50.0+{eps}, got {child_width}"
  );
  assert!(
    child_width < min_content - eps,
    "overflow: hidden should not use min-content automatic min size; expected < {min_content}-{eps}, got {child_width}"
  );
}
