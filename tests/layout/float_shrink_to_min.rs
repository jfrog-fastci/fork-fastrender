use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

/// Floats with `width: auto` should use the CSS shrink-to-fit formula and then
/// honor `min-width`/`max-width` caps. When the available width is smaller than
/// the authored minimum, the used width must clamp to the min-width instead of
/// collapsing to the available space.
#[test]
fn float_auto_width_honors_min_width() {
  // Container 100px wide with a single floating child that has only a min-width.
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

  // The float should clamp up to its min-width even though the available width is smaller.
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
  let text_child = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "hi".to_string(),
  );
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![
    text_child,
  ]);

  let container = BoxNode::new_block(container_style, FormattingContextType::Block, vec![float_box]);

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc.layout(&container, &constraints).expect("layout should succeed");

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
  // Float with text content and a max-width smaller than its intrinsic size.
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
  let container = BoxNode::new_block(container_style, FormattingContextType::Block, vec![float_box]);

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc.layout(&container, &constraints).expect("layout should succeed");

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
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  // Find the float fragment (it may not be a direct child due to float reparenting).
  fn find_float<'a>(node: &'a fastrender::tree::fragment_tree::FragmentNode) -> Option<&'a fastrender::tree::fragment_tree::FragmentNode> {
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
