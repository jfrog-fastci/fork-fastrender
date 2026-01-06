use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_width_max_content_sizes_to_text() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(300.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

  let text = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "Hello world".to_string(),
  );
  let child_box = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![text],
  );

  let expected = BlockFormattingContext::new()
    .compute_intrinsic_inline_size(&child_box, IntrinsicSizingMode::MaxContent)
    .expect("max-content inline size");

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_box],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.first().expect("child fragment");
  let width = child_fragment.bounds.width();
  assert!(
    (width - expected).abs() < 0.5,
    "expected width:max-content to resolve to {expected:.2}, got {width:.2}"
  );
}

#[test]
fn flex_item_width_fit_content_clamps_and_shrinks_with_siblings() {
  // Two items whose total base size overflows the container.
  // - Item A uses width: fit-content and should clamp its base size to the available space.
  // - Item B is fixed width.
  // This should affect the shrink distribution.
  let container_width = 100.0;
  let fixed_width = 50.0;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(container_width));

  let mut item_a_style = ComputedStyle::default();
  item_a_style.display = Display::Block;
  item_a_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
  let text = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "This is some long text".to_string(),
  );
  let item_a = BoxNode::new_block(
    Arc::new(item_a_style),
    FormattingContextType::Block,
    vec![text],
  );

  let mut item_b_style = ComputedStyle::default();
  item_b_style.display = Display::Block;
  item_b_style.width = Some(Length::px(fixed_width));
  let item_b = BoxNode::new_block(Arc::new(item_b_style), FormattingContextType::Block, vec![]);

  let block_fc = BlockFormattingContext::new();
  let min_a = block_fc
    .compute_intrinsic_inline_size(&item_a, IntrinsicSizingMode::MinContent)
    .expect("min-content inline size");
  let max_a = block_fc
    .compute_intrinsic_inline_size(&item_a, IntrinsicSizingMode::MaxContent)
    .expect("max-content inline size");

  assert!(
    min_a < container_width,
    "expected min-content to be smaller than the available space (min_a={min_a:.2})"
  );
  assert!(
    max_a > container_width,
    "expected max-content to exceed the available space (max_a={max_a:.2})"
  );

  let fit_a = max_a.min(min_a.max(container_width));
  let total_base = fit_a + fixed_width;
  let expected_a = container_width * fit_a / total_base;
  let expected_b = container_width * fixed_width / total_base;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(container_width),
        AvailableSpace::Indefinite,
      ),
    )
    .expect("layout should succeed");

  let a_width = fragment.children[0].bounds.width();
  let b_width = fragment.children[1].bounds.width();

  assert!(
    (a_width - expected_a).abs() < 0.5,
    "expected fit-content item to shrink to ~{expected_a:.2}, got {a_width:.2}"
  );
  assert!(
    (b_width - expected_b).abs() < 0.5,
    "expected fixed item to shrink to ~{expected_b:.2}, got {b_width:.2}"
  );
}

#[test]
fn flex_item_width_fit_content_clamps_against_container_width_when_constraints_are_wider() {
  // This simulates the flex formatting context being invoked at the root of the layout tree:
  // the *available* width can be larger than the flex container's own definite `width`.
  //
  // `fit-content` on flex items should clamp against the flex container's content box size, not
  // the containing block's available space.
  let containing_width = 200.0;
  let container_width = 100.0;
  let fixed_width = 90.0;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(container_width));

  let mut item_a_style = ComputedStyle::default();
  item_a_style.display = Display::Block;
  item_a_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
  let text = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "word word word word word word word word word".to_string(),
  );
  let item_a = BoxNode::new_block(
    Arc::new(item_a_style),
    FormattingContextType::Block,
    vec![text],
  );

  let mut item_b_style = ComputedStyle::default();
  item_b_style.display = Display::Block;
  item_b_style.width = Some(Length::px(fixed_width));
  let item_b = BoxNode::new_block(Arc::new(item_b_style), FormattingContextType::Block, vec![]);

  let block_fc = BlockFormattingContext::new();
  let min_a = block_fc
    .compute_intrinsic_inline_size(&item_a, IntrinsicSizingMode::MinContent)
    .expect("min-content inline size");
  let max_a = block_fc
    .compute_intrinsic_inline_size(&item_a, IntrinsicSizingMode::MaxContent)
    .expect("max-content inline size");

  assert!(
    min_a < container_width,
    "expected min-content to be smaller than the container width (min_a={min_a:.2})"
  );
  assert!(
    max_a > container_width,
    "expected max-content to exceed the container width (max_a={max_a:.2})"
  );

  let fit_a = max_a.min(min_a.max(container_width));
  let total_base = fit_a + fixed_width;
  let expected_a = container_width * fit_a / total_base;
  let expected_b = container_width * fixed_width / total_base;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(containing_width),
        AvailableSpace::Indefinite,
      ),
    )
    .expect("layout should succeed");

  let a_width = fragment.children[0].bounds.width();
  let b_width = fragment.children[1].bounds.width();

  assert!(
    ((a_width + b_width) - container_width).abs() < 0.5,
    "expected flex items to fill the container's main size ({container_width:.2}); got a+b={:.2}",
    a_width + b_width
  );

  assert!(
    (a_width - expected_a).abs() < 0.5,
    "expected fit-content item to shrink to ~{expected_a:.2}, got {a_width:.2}"
  );
  assert!(
    (b_width - expected_b).abs() < 0.5,
    "expected fixed item to shrink to ~{expected_b:.2}, got {b_width:.2}"
  );
}

#[test]
fn flex_item_width_max_content_rebases_percent_padding_and_borders() {
  let container_width = 300.0;

  let mut flex_container_style = ComputedStyle::default();
  flex_container_style.display = Display::Flex;
  flex_container_style.width = Some(Length::px(container_width));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  // Prevent flexbox from shrinking the item back to the container width; we want to validate the
  // resolved max-content *base* size (which may legitimately overflow).
  child_style.flex_shrink = 0.0;
  child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  child_style.padding_left = Length::percent(10.0);
  child_style.padding_right = Length::px(5.0);
  child_style.border_left_style = BorderStyle::Solid;
  child_style.border_right_style = BorderStyle::Solid;
  child_style.border_left_width = Length::px(2.0);
  child_style.border_right_width = Length::px(2.0);

  let text = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "word ".repeat(20),
  );
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![text],
  );

  // Compute the expected max-content border-box size by taking the intrinsic border-box size
  // computed with a 0px percentage base and rebasing the percentage padding/border edges against
  // the actual containing block width.
  let mut measure_style = child.style.as_ref().clone();
  measure_style.width_keyword = None;
  let measure = BoxNode::new_block(
    Arc::new(measure_style),
    FormattingContextType::Block,
    child.children.clone(),
  );
  let block_fc = BlockFormattingContext::new();
  let intrinsic_base0 = block_fc
    .compute_intrinsic_inline_size(&measure, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic max-content size");
  let edges = |percentage_base: f32| -> f32 {
    measure
      .style
      .padding_left
      .resolve_against(percentage_base)
      .unwrap_or(0.0)
      + measure
        .style
        .padding_right
        .resolve_against(percentage_base)
        .unwrap_or(0.0)
      + measure
        .style
        .used_border_left_width()
        .resolve_against(percentage_base)
        .unwrap_or(0.0)
      + measure
        .style
        .used_border_right_width()
        .resolve_against(percentage_base)
        .unwrap_or(0.0)
  };
  let edges_base0 = edges(0.0);
  let edges_actual = edges(container_width);
  let expected = (intrinsic_base0 - edges_base0 + edges_actual).max(0.0);

  let flex_container = BoxNode::new_block(
    Arc::new(flex_container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  let flex_fc = FlexFormattingContext::new();
  let fragment = flex_fc
    .layout(
      &flex_container,
      &LayoutConstraints::new(AvailableSpace::Definite(container_width), AvailableSpace::Indefinite),
    )
    .expect("flex layout should succeed");
  let actual = fragment.children[0].bounds.width();

  assert!(
    (actual - expected).abs() < 0.5,
    "expected flex max-content width to include rebased percentage padding (expected {expected:.2}, got {actual:.2})"
  );
}
