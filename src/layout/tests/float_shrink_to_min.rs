use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

#[test]
fn float_auto_width_honors_min_width() {
  let container_style = Arc::new(ComputedStyle::default());

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.min_width = Some(Length::px(150.0));
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let float_fragment = &fragment.children[0];
  assert!((float_fragment.bounds.width() - 150.0).abs() < 0.01);
}

/// When the intrinsic shrink-to-fit width is smaller than `min-width`, the used width should clamp
/// up to the authored minimum instead of shrinking to the content.
#[test]
fn float_auto_width_with_content_clamps_to_min_width() {
  let container_style = Arc::new(ComputedStyle::default());

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.min_width = Some(Length::px(80.0));
  let text_child = BoxNode::new_text(Arc::new(ComputedStyle::default()), "hi".to_string());
  let float_box = BoxNode::new_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![text_child],
  );

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let float_fragment = &fragment.children[0];
  assert!(
    (float_fragment.bounds.width() - 80.0).abs() < 0.5,
    "float width should clamp to min-width; got {:.2}",
    float_fragment.bounds.width()
  );
}

/// Floats should also clamp shrink-to-fit results to max-width. If the preferred
/// widths exceed the authored max-width, the used width must not overflow it.
#[test]
fn float_auto_width_clamps_to_max_width() {
  let container_style = Arc::new(ComputedStyle::default());

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.max_width = Some(Length::px(50.0));
  let text_child = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "Hello world".to_string(),
  );
  let float_box = BoxNode::new_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![text_child],
  );

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let float_fragment = &fragment.children[0];
  assert!(float_fragment.bounds.width() <= 50.0 + 0.01);
}

/// Shrink-to-fit floats must not clamp their used width to the available space when the
/// min-content width is larger than that space (CSS 2.1 §10.3.5).
///
/// This can happen with "unbreakable" content (e.g. a long word / fixed-width child).
#[test]
fn float_auto_width_can_exceed_containing_block_when_intrinsic_min_exceeds_available() {
  let container_style = Arc::new(ComputedStyle::default());

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;

  // Simulate an unbreakable item by using a fixed-width child.
  let mut child_style = ComputedStyle::default();
  child_style.width = Some(Length::px(200.0));
  child_style.height = Some(Length::px(10.0));
  let child_box = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let float_box = BoxNode::new_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![child_box],
  );
  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let float_fragment = &fragment.children[0];
  assert!(
    (float_fragment.bounds.width() - 200.0).abs() < 0.5,
    "float should size to its intrinsic min/max (200px), not clamp to available width; got {:.2}",
    float_fragment.bounds.width()
  );
}

/// When a block does not establish a new BFC, it inherits the ancestor float context.
/// Floats inside that block must still be positioned relative to the *block's* containing
/// block width (not the ancestor's).
#[test]
fn inherited_float_context_is_scoped_to_containing_block_width() {
  let root_style = Arc::new(ComputedStyle::default());

  let mut inner_style = ComputedStyle::default();
  inner_style.width = Some(Length::px(100.0));
  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(20.0));
  float_style.height = Some(Length::px(10.0));
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Block,
    vec![float_box],
  );
  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![inner]);

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  // Find the float fragment (it may not be a direct child due to float reparenting).
  fn find_float<'a>(
    node: &'a fastrender::tree::fragment_tree::FragmentNode,
  ) -> Option<&'a fastrender::tree::fragment_tree::FragmentNode> {
    if let Some(style) = node.style.as_ref() {
      if style.float == Float::Right && (node.bounds.width() - 20.0).abs() < 0.5 {
        return Some(node);
      }
    }
    node.children.iter().find_map(find_float)
  }

  let float_fragment = find_float(&fragment).expect("expected to find float fragment");
  assert!(
    (float_fragment.bounds.x() - 80.0).abs() < 0.5,
    "expected nested float:right to align to 100px containing block (x≈80); got x={:.2}",
    float_fragment.bounds.x()
  );
}

#[test]
fn float_auto_width_includes_floated_children_in_intrinsic_widths() {
  let container_style = Arc::new(ComputedStyle::default());

  let mut inner_style = ComputedStyle::default();
  inner_style.float = Float::Left;
  inner_style.width = Some(Length::px(200.0));
  let inner_box = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(
      Arc::new(ComputedStyle::default()),
      "x".to_string(),
    )],
  );

  let mut outer_style = ComputedStyle::default();
  outer_style.float = Float::Left;
  let outer_box = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner_box],
  );

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![outer_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(500.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let outer_fragment = &fragment.children[0];
  assert!(
    (outer_fragment.bounds.width() - 200.0).abs() < 0.01,
    "expected outer float to shrink-to-fit around inner float width"
  );
}

/// Shrink-to-fit "available width" for floats is based on the containing block width, not the
/// remaining space next to previously-placed floats. If a float doesn't fit, it should drop below
/// earlier floats rather than shrinking and wrapping its own contents (CSS 2.1 §10.3.5).
#[test]
fn float_auto_width_does_not_shrink_to_remaining_space() {
  // Container 150px wide with:
  // - a first float 80px wide
  // - a second float whose max-content width is ~120px (two 60px inline blocks)
  //
  // The second float cannot fit next to the first, so it should keep its max-content width and
  // move down to the next float line.
  let container_style = Arc::new(ComputedStyle::default());

  let mut first_style = ComputedStyle::default();
  first_style.float = Float::Left;
  first_style.width = Some(Length::px(80.0));
  first_style.height = Some(Length::px(10.0));
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut inline_a_style = ComputedStyle::default();
  inline_a_style.display = fastrender::style::display::Display::InlineBlock;
  inline_a_style.width = Some(Length::px(60.0));
  inline_a_style.height = Some(Length::px(10.0));
  let inline_a = BoxNode::new_inline_block(
    Arc::new(inline_a_style),
    FormattingContextType::Block,
    vec![],
  );

  let space = BoxNode::new_text(Arc::new(ComputedStyle::default()), " ".to_string());

  let mut inline_b_style = ComputedStyle::default();
  inline_b_style.display = fastrender::style::display::Display::InlineBlock;
  inline_b_style.width = Some(Length::px(60.0));
  inline_b_style.height = Some(Length::px(10.0));
  let inline_b = BoxNode::new_inline_block(
    Arc::new(inline_b_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut second_style = ComputedStyle::default();
  second_style.float = Float::Left;
  let second = BoxNode::new_block(
    Arc::new(second_style),
    FormattingContextType::Block,
    vec![inline_a, space, inline_b],
  );

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![first, second],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(150.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let second_fragment = &fragment.children[1];

  assert!(
    second_fragment.bounds.width() > 100.0,
    "expected second float to keep its max-content width; got {:.2}",
    second_fragment.bounds.width()
  );
  assert!(
    second_fragment.bounds.y() > 0.0,
    "expected second float to drop below the first; got y={:.2}",
    second_fragment.bounds.y()
  );
}
