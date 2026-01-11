use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn abspos_child_origin_is_relative_to_padding_box_not_content_box() {
  // Regression test for positioned descendants in a padded block formatting context.
  //
  // When a block has non-zero padding, its in-flow children are translated into the fragment's
  // border-box coordinate space. Out-of-flow positioned children must receive the same
  // translation; otherwise `top: 0; left: 0` is offset into negative coordinates and can paint
  // outside the containing block (e.g. the IETF jumbotron ::before overlay darkening the header).

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(150.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("layout should succeed");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute positioned fragment present");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1,
    "expected abspos child x to align with the padding edge (got x = {})",
    abs_fragment.bounds.x()
  );
  assert!(
    abs_fragment.bounds.y().abs() < 0.1,
    "expected abspos child y to align with the padding edge (got y = {})",
    abs_fragment.bounds.y()
  );
}

#[test]
fn abspos_inset_stretch_fills_padding_box_without_extra_padding_offset() {
  // Regression test: `inset: 0` (top/right/bottom/left all set) with auto size should fill the
  // containing block's padding box. This is commonly used for overlay pseudo-elements:
  //
  //   .hero::before { position: absolute; inset: 0; background: rgba(...); }
  //
  // On ietf.org this overlay was shifted by the parent's padding, leaving the hero image undimmed.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(150.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Inline;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));

  let abs_child = BoxNode::new_inline(Arc::new(abs_style), vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("layout should succeed");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute positioned fragment present");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1,
    "expected inset stretch child to start at padding edge (got x = {})",
    abs_fragment.bounds.x()
  );
  assert!(
    abs_fragment.bounds.y().abs() < 0.1,
    "expected inset stretch child to start at padding edge (got y = {})",
    abs_fragment.bounds.y()
  );
  assert!(
    (abs_fragment.bounds.width() - fragment.bounds.width()).abs() < 0.1,
    "expected inset stretch child to fill padding box width (expected {}, got {})",
    fragment.bounds.width(),
    abs_fragment.bounds.width()
  );
  assert!(
    (abs_fragment.bounds.height() - fragment.bounds.height()).abs() < 0.1,
    "expected inset stretch child to fill padding box height (expected {}, got {})",
    fragment.bounds.height(),
    abs_fragment.bounds.height()
  );
}

#[test]
fn abspos_inset_stretch_with_auto_height_and_in_flow_content_is_not_offset_by_padding() {
  // Matches a common real-world pattern: an auto-height hero container with padding and an
  // absolutely positioned overlay pseudo-element (`inset: 0`) plus normal in-flow content.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Inline;
  overlay_style.position = Position::Absolute;
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.right = InsetValue::Length(Length::px(0.0));
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.bottom = InsetValue::Length(Length::px(0.0));
  let overlay = BoxNode::new_inline(Arc::new(overlay_style), vec![]);

  let mut content_style = ComputedStyle::default();
  content_style.display = Display::Block;
  content_style.height = Some(Length::px(50.0));
  let content = BoxNode::new_block(Arc::new(content_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![overlay, content],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("layout should succeed");

  let overlay_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute overlay fragment present");

  assert!(
    overlay_fragment.bounds.x().abs() < 0.1,
    "expected overlay x to align with padding edge (got x = {})",
    overlay_fragment.bounds.x()
  );
  assert!(
    overlay_fragment.bounds.y().abs() < 0.1,
    "expected overlay y to align with padding edge (got y = {})",
    overlay_fragment.bounds.y()
  );
}

#[test]
fn abspos_child_origin_is_relative_to_padding_box_when_container_is_block_child() {
  // `BlockFormattingContext::layout` uses a separate `layout_block_child` code path for nested
  // blocks. Ensure absolute-positioned children still use the padding box of their nearest
  // positioned ancestor in that nested path.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(150.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![container],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("layout should succeed");

  let container_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Relative)))
    .expect("container fragment present");
  let abs_fragment = container_fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute positioned fragment present");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1,
    "expected abspos child x to align with the padding edge (got x = {})",
    abs_fragment.bounds.x()
  );
  assert!(
    abs_fragment.bounds.y().abs() < 0.1,
    "expected abspos child y to align with the padding edge (got y = {})",
    abs_fragment.bounds.y()
  );
}

#[test]
fn viewport_fixed_children_do_not_inherit_padding_translation() {
  // `position: fixed` descendants without a fixed-containing-block ancestor are laid out against
  // the viewport and stored in absolute (viewport) coordinates. Ensure the block-child out-of-flow
  // positioning path does not translate those fragments by the parent's padding, which would
  // incorrectly offset fixed overlays.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(100.0));
  let spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(150.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut fixed_style = ComputedStyle::default();
  fixed_style.display = Display::Block;
  fixed_style.position = Position::Fixed;
  fixed_style.left = InsetValue::Length(Length::px(0.0));
  fixed_style.top = InsetValue::Length(Length::px(0.0));
  fixed_style.width = Some(Length::px(10.0));
  fixed_style.height = Some(Length::px(10.0));

  let fixed_child =
    BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![fixed_child],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![spacer, container],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("layout should succeed");

  let container_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Relative)))
    .expect("container fragment present");
  let fixed_fragment = container_fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Fixed)))
    .expect("fixed positioned fragment present");

  assert!(
    fixed_fragment.bounds.x().abs() < 0.1,
    "expected fixed child x to remain viewport-relative (got x = {})",
    fixed_fragment.bounds.x()
  );
  assert!(
    fixed_fragment.bounds.y().abs() < 0.1,
    "expected fixed child y to remain viewport-relative (got y = {})",
    fixed_fragment.bounds.y()
  );
}
