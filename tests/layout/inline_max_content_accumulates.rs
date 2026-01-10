use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

/// Shrink-to-fit widths must use the inline formatting context's *max-content* size, which is the
/// width of the contents when only forced line breaks are honored (CSS 2.1 §10.3.5).
///
/// UAX#14 reports a mandatory break at end-of-text as a sentinel, but that must not behave like a
/// forced break during intrinsic sizing; otherwise the max-content measurement becomes the maximum
/// width of each adjacent inline item rather than the sum, collapsing float widths in common
/// "icon + label" patterns.
#[test]
fn float_shrink_to_fit_sums_inline_items_across_collapsed_space() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  let container_style = Arc::new(container_style);

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Block;
  outer_style.float = Float::Left;
  let outer_style = Arc::new(outer_style);

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::InlineBlock;
  a_style.width = Some(Length::px(30.0));
  let inline_a = BoxNode::new_inline_block(Arc::new(a_style), FormattingContextType::Block, vec![]);

  // A whitespace-only text node should produce a single collapsed space between inline items.
  let mut space_style = ComputedStyle::default();
  space_style.display = Display::Inline;
  let space = BoxNode::new_text(Arc::new(space_style), " ".to_string());

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::InlineBlock;
  b_style.width = Some(Length::px(40.0));
  let inline_b = BoxNode::new_inline_block(Arc::new(b_style), FormattingContextType::Block, vec![]);

  let outer = BoxNode::new_block(
    outer_style,
    FormattingContextType::Block,
    vec![inline_a, space, inline_b],
  );

  let container = BoxNode::new_block(container_style, FormattingContextType::Block, vec![outer]);

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(500.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let outer_fragment = &fragment.children[0];
  assert!(
    outer_fragment.bounds.width() >= 69.0,
    "expected shrink-to-fit float to size to sum of inline items; got {:.2}",
    outer_fragment.bounds.width()
  );
}

