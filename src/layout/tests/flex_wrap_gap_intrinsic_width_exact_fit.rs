use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::AlignItems;
use crate::style::types::FlexDirection;
use crate::style::types::FlexWrap;
use crate::style::types::JustifyContent;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use std::sync::Arc;

fn fixed_block(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Ensure the items' hypothetical main sizes are driven by the authored sizes.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

fn find_child<'a>(
  fragment: &'a crate::FragmentNode,
  box_id: usize,
) -> &'a crate::FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
}

#[test]
fn flex_wrap_gap_exact_fit_does_not_wrap_when_container_is_intrinsically_sized() {
  // Regression: a nested `flex-wrap: wrap` container that is sized via an intrinsic-width probe
  // (common for `align-items:center` in a column flex container) must not wrap items when their
  // widths + gap fit exactly into the resolved width.
  //
  // Inspired by github.com's `.CtaForm` (wrap + gap + shrink-to-fit), but uses fixed item sizes to
  // assert that Taffy's line breaking does not wrap on an exact fit.
  let fc = FlexFormattingContext::new();

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Column;
  outer_style.align_items = AlignItems::Center;
  outer_style.width = Some(Length::px(1000.0));
  outer_style.width_keyword = None;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Flex;
  inner_style.flex_wrap = FlexWrap::Wrap;
  inner_style.justify_content = JustifyContent::Center;
  inner_style.align_items = AlignItems::FlexEnd;
  inner_style.grid_column_gap = Length::px(16.0);
  inner_style.grid_row_gap = Length::px(16.0);

  let mut item_a = BoxNode::new_block(
    fixed_block(474.0, 56.0),
    FormattingContextType::Block,
    vec![],
  );
  item_a.id = 3;
  let mut item_b = BoxNode::new_block(
    fixed_block(236.0, 56.0),
    FormattingContextType::Block,
    vec![],
  );
  item_b.id = 4;

  let mut inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );
  inner.id = 2;

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![inner],
  );

  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite_width(1000.0))
    .expect("layout succeeds");

  let inner_fragment = find_child(&fragment, 2);
  assert!(
    inner_fragment.bounds.width() >= 726.0 - 0.1,
    "expected intrinsic width to be ~726px, got {:.2}",
    inner_fragment.bounds.width()
  );

  let first = find_child(inner_fragment, 3);
  let second = find_child(inner_fragment, 4);

  assert!(
    first.bounds.y().abs() < 1e-3,
    "expected first item on first line, got y={:.2}",
    first.bounds.y()
  );
  assert!(
    second.bounds.y().abs() < 1e-3,
    "expected second item to remain on the first line (no wrap), got y={:.2}",
    second.bounds.y()
  );

  let expected_x = 474.0 + 16.0;
  assert!(
    (second.bounds.x() - expected_x).abs() < 0.6,
    "expected second item x≈{expected_x}, got {:.2}",
    second.bounds.x()
  );
}

#[test]
fn flex_wrap_gap_subpixel_fit_does_not_wrap_when_container_is_intrinsically_sized() {
  // Regression: intrinsic-width probes must preserve legitimate subpixel widths. Rounding the
  // probed width to a whole pixel can make a shrink-to-fit flex container slightly too small,
  // causing `flex-wrap` to wrap items that fit in un-snapped CSS pixels.
  let fc = FlexFormattingContext::new();

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Column;
  outer_style.align_items = AlignItems::Center;
  outer_style.width = Some(Length::px(1000.0));
  outer_style.width_keyword = None;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Flex;
  inner_style.flex_wrap = FlexWrap::Wrap;
  inner_style.justify_content = JustifyContent::Center;
  inner_style.align_items = AlignItems::FlexEnd;

  let mut item_a = BoxNode::new_block(
    fixed_block(308.6, 56.0),
    FormattingContextType::Block,
    vec![],
  );
  item_a.id = 3;
  let mut item_b = BoxNode::new_block(
    fixed_block(308.6, 56.0),
    FormattingContextType::Block,
    vec![],
  );
  item_b.id = 4;

  let mut inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );
  inner.id = 2;

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![inner],
  );

  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite_width(1000.0))
    .expect("layout succeeds");

  let inner_fragment = find_child(&fragment, 2);
  assert!(
    inner_fragment.bounds.width() >= 617.1,
    "expected intrinsic width to preserve subpixel fit, got {:.2}",
    inner_fragment.bounds.width()
  );

  let first = find_child(inner_fragment, 3);
  let second = find_child(inner_fragment, 4);

  assert!(
    first.bounds.y().abs() < 1e-3,
    "expected first item on first line, got y={:.2}",
    first.bounds.y()
  );
  assert!(
    second.bounds.y().abs() < 1e-3,
    "expected second item to remain on the first line (no wrap), got y={:.2}",
    second.bounds.y()
  );
}
