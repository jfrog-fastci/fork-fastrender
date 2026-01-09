use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{FlexDirection, FlexWrap};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn flex_wrap_auto_height_expands_to_fit_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.grid_column_gap = Length::px(10.0);
  container_style.grid_row_gap = Length::px(5.0);

  fn item_style(width: f32, height: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::px(width));
    style.height = Some(Length::px(height));
    style.width_keyword = None;
    style.height_keyword = None;
    // Keep widths fixed so wrapping is driven by the container width + gap.
    style.flex_shrink = 0.0;
    Arc::new(style)
  }

  let mut item1 = BoxNode::new_block(
    item_style(40.0, 50.0),
    FormattingContextType::Block,
    vec![],
  );
  item1.id = 1;

  let mut item2 = BoxNode::new_block(
    item_style(40.0, 50.0),
    FormattingContextType::Block,
    vec![],
  );
  item2.id = 2;

  // Third item wraps onto a second line with a smaller cross size.
  let mut item3 = BoxNode::new_block(
    item_style(40.0, 30.0),
    FormattingContextType::Block,
    vec![],
  );
  item3.id = 3;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item1, item2, item3],
  );

  // Indefinite height models `height:auto` sizing to content.
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  // Two 40px items + 10px column gap fit on the first line; the third wraps. The container's
  // auto height should be the sum of line cross sizes plus the row gap:
  // 50 (line 1) + 5 (row gap) + 30 (line 2) = 85.
  let eps = 1e-3;
  assert!(
    (fragment.bounds.height() - 85.0).abs() < eps,
    "expected flex container auto height to include all wrapped lines, got {}",
    fragment.bounds.height()
  );
}
