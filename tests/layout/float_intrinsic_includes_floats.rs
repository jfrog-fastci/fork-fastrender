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

/// Intrinsic inline size calculation must account for floated descendants.
///
/// A common pattern (e.g. Bootstrap nav bars) is a shrink-to-fit float whose contents are *also*
/// floats. If floats are ignored during intrinsic measurement, the outer float's min/max-content
/// widths collapse to 0px and the element is positioned as a 0px-wide float at the container edge.
#[test]
fn float_shrink_to_fit_includes_floated_children() {
  // Container 500px wide with an outer float that contains only another float.
  let container_style = Arc::new(ComputedStyle::default());

  let mut inner_style = ComputedStyle::default();
  inner_style.float = Float::Left;
  inner_style.width = Some(Length::px(100.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = ComputedStyle::default();
  outer_style.float = Float::Right;
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner],
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
    (outer_fragment.bounds.width() - 100.0).abs() < 0.01,
    "expected shrink-to-fit float to size to floated child width; got {:.2}",
    outer_fragment.bounds.width()
  );
}

#[test]
fn float_shrink_to_fit_sums_multiple_floats() {
  // Container 500px wide with an outer float whose contents are multiple floats.
  let container_style = Arc::new(ComputedStyle::default());

  let mut inner_style_a = ComputedStyle::default();
  inner_style_a.float = Float::Left;
  inner_style_a.width = Some(Length::px(100.0));
  let inner_a = BoxNode::new_block(
    Arc::new(inner_style_a),
    FormattingContextType::Block,
    vec![],
  );

  let mut inner_style_b = ComputedStyle::default();
  inner_style_b.float = Float::Left;
  inner_style_b.width = Some(Length::px(80.0));
  let inner_b = BoxNode::new_block(
    Arc::new(inner_style_b),
    FormattingContextType::Block,
    vec![],
  );

  let mut outer_style = ComputedStyle::default();
  outer_style.float = Float::Right;
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner_a, inner_b],
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
    (outer_fragment.bounds.width() - 180.0).abs() < 0.01,
    "expected shrink-to-fit float to size to sum of floated child widths; got {:.2}",
    outer_fragment.bounds.width()
  );
}

#[test]
fn float_shrink_to_fit_sums_inline_level_floated_children() {
  // Bootstrap-style button groups use `display:inline-block` buttons that are also floated.
  // Floated inline-level boxes must still contribute to intrinsic sizing; otherwise the parent's
  // shrink-to-fit width collapses to 0.
  let container_style = Arc::new(ComputedStyle::default());

  let mut inner_style_a = ComputedStyle::default();
  inner_style_a.display = Display::InlineBlock;
  inner_style_a.float = Float::Left;
  inner_style_a.width = Some(Length::px(100.0));
  let inner_a = BoxNode::new_inline_block(
    Arc::new(inner_style_a),
    FormattingContextType::Block,
    vec![],
  );

  let mut inner_style_b = ComputedStyle::default();
  inner_style_b.display = Display::InlineBlock;
  inner_style_b.float = Float::Left;
  inner_style_b.width = Some(Length::px(80.0));
  let inner_b = BoxNode::new_inline_block(
    Arc::new(inner_style_b),
    FormattingContextType::Block,
    vec![],
  );

  let mut outer_style = ComputedStyle::default();
  outer_style.float = Float::Right;
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner_a, inner_b],
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
    (outer_fragment.bounds.width() - 180.0).abs() < 0.01,
    "expected shrink-to-fit float to size to sum of floated inline children; got {:.2}",
    outer_fragment.bounds.width()
  );
}
