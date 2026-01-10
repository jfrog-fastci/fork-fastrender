use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn percent_height_resolves_inside_definite_flex_item_main_size() {
  // Regression test:
  // - A flex container with a definite height (main axis for `flex-direction: column`) assigns a
  //   definite used height to its flex items even when the items themselves are `height:auto`.
  // - Percentage heights inside those flex items should therefore resolve against the flex item's
  //   used height (CSS2.1 §10.5, plus flexbox's definite-size propagation rules).
  //
  // This mirrors real-world patterns like nasa.gov where a `.hds-cover-wrapper { height: 100% }`
  // element sits inside a flex item whose main size comes from flexing, not an authored `height`.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.flex_grow = 1.0;
  item_style.flex_shrink = 1.0;

  let mut cover_style = ComputedStyle::default();
  cover_style.display = Display::Block;
  cover_style.height = Some(Length::percent(100.0));

  let cover1 = BoxNode::new_block(
    Arc::new(cover_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let cover2 = BoxNode::new_block(Arc::new(cover_style), FormattingContextType::Block, vec![]);

  let item1 = BoxNode::new_block(
    Arc::new(item_style.clone()),
    FormattingContextType::Block,
    vec![cover1],
  );
  let item2 = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![cover2],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item1, item2],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(200.0),
      ),
    )
    .expect("layout should succeed");

  // Each flex item should receive half the container height (~100px).
  let item_fragment = fragment.children.first().expect("flex item fragment");
  let cover_fragment = item_fragment.children.first().expect("cover fragment");

  let item_h = item_fragment.bounds.height();
  let cover_h = cover_fragment.bounds.height();

  assert!(
    (item_h - 100.0).abs() < 0.5,
    "expected flex item to be ~100px tall, got {item_h}"
  );
  assert!(
    (cover_h - item_h).abs() < 0.5,
    "expected `height:100%` child to fill flex item ({}px), got {cover_h}",
    item_h
  );
}
