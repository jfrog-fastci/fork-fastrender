use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AlignItems;
use fastrender::style::types::Direction;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

fn find_fragment_with_id<'a>(
  fragment: &'a fastrender::FragmentNode,
  id: usize,
) -> Option<&'a fastrender::FragmentNode> {
  if fragment.box_id().is_some_and(|fragment_id| fragment_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn layout_flex(
  container_id: usize,
  container_style: ComputedStyle,
  children: Vec<BoxNode>,
  width: f32,
  height: f32,
) -> fastrender::FragmentNode {
  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );
  container.id = container_id;
  let fc = FlexFormattingContext::new();
  fc.layout(&container, &LayoutConstraints::definite(width, height))
    .expect("layout succeeds")
}

fn sized_child(id: usize, width: f32, height: f32) -> BoxNode {
  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(width));
  child_style.height = Some(Length::px(height));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = id;
  child
}

#[test]
fn flex_align_items_flex_start_vertical_rl_respects_axis_polarity() {
  let container_id = 10_000;
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(40.0));
  container_style.align_items = AlignItems::FlexStart;

  let child_id = 10_001;
  let child = sized_child(child_id, 10.0, 10.0);

  let fragment = layout_flex(container_id, container_style.clone(), vec![child], 100.0, 40.0);
  let child_fragment = find_fragment_with_id(&fragment, child_id).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    90.0,
    "vertical-rl block-axis is right→left so flex-start should align to the right",
  );

  container_style.align_items = AlignItems::FlexEnd;
  let child = sized_child(child_id, 10.0, 10.0);
  let fragment = layout_flex(container_id, container_style, vec![child], 100.0, 40.0);
  let child_fragment = find_fragment_with_id(&fragment, child_id).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "vertical-rl block-axis is right→left so flex-end should align to the left",
  );
}

#[test]
fn flex_align_items_flex_start_rtl_column_respects_axis_polarity() {
  let container_id = 20_000;
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::HorizontalTb;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(40.0));
  container_style.align_items = AlignItems::FlexStart;

  let child_id = 20_001;
  let child = sized_child(child_id, 10.0, 10.0);

  let fragment = layout_flex(container_id, container_style, vec![child], 100.0, 40.0);
  let child_fragment = find_fragment_with_id(&fragment, child_id).expect("child fragment");

  assert_approx(
    child_fragment.bounds.x(),
    90.0,
    "rtl inline-axis is right→left so flex-start should align to the right",
  );
}

#[test]
fn flex_align_items_flex_start_wrap_reverse_flips_even_when_axis_negative() {
  let container_id = 30_000;
  let mut base_style = ComputedStyle::default();
  base_style.display = Display::Flex;
  base_style.writing_mode = WritingMode::VerticalRl;
  base_style.flex_direction = FlexDirection::Row;
  base_style.width = Some(Length::px(100.0));
  base_style.height = Some(Length::px(20.0));
  base_style.align_items = AlignItems::FlexStart;

  // Ensure wrapping produces multiple flex lines (2 columns in vertical writing mode).
  let wide_id = 30_001;
  let narrow_id = 30_002;
  let extra_id = 30_003;
  let wide = sized_child(wide_id, 50.0, 10.0);
  let narrow = sized_child(narrow_id, 10.0, 10.0);
  let extra = sized_child(extra_id, 10.0, 10.0);

  // No wrap-reverse: cross-start is on the right in vertical-rl, so flex-start aligns right edges.
  let mut wrap_style = base_style.clone();
  wrap_style.flex_wrap = FlexWrap::Wrap;
  let fragment = layout_flex(
    container_id,
    wrap_style,
    vec![wide.clone(), narrow.clone(), extra.clone()],
    100.0,
    20.0,
  );
  let wide_fragment = find_fragment_with_id(&fragment, wide_id).expect("wide fragment");
  let narrow_fragment = find_fragment_with_id(&fragment, narrow_id).expect("narrow fragment");
  assert_approx(
    narrow_fragment.bounds.x() - wide_fragment.bounds.x(),
    40.0,
    "without wrap-reverse, flex-start should align items to the cross-start (right) edge",
  );

  // With wrap-reverse: cross-start is flipped, so flex-start aligns left edges.
  let mut wrap_reverse_style = base_style;
  wrap_reverse_style.flex_wrap = FlexWrap::WrapReverse;
  let fragment = layout_flex(
    container_id,
    wrap_reverse_style,
    vec![wide, narrow, extra],
    100.0,
    20.0,
  );
  let wide_fragment = find_fragment_with_id(&fragment, wide_id).expect("wide fragment");
  let narrow_fragment = find_fragment_with_id(&fragment, narrow_id).expect("narrow fragment");
  assert_approx(
    narrow_fragment.bounds.x() - wide_fragment.bounds.x(),
    0.0,
    "wrap-reverse should flip cross-start/cross-end even when the cross axis runs negative",
  );
}
