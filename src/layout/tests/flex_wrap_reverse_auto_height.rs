use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::{FlexDirection, FlexWrap};
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn flex_wrap_reverse_auto_height_expands_to_fit_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.grid_column_gap = Length::px(10.0);
  container_style.grid_row_gap = Length::px(5.0);

  fn item_style(width: f32, height: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::px(width));
    style.height = Some(Length::px(height));
    style.width_keyword = None;
    style.height_keyword = None;
    style.flex_shrink = 0.0;
    Arc::new(style)
  }

  let mut item1 = BoxNode::new_block(item_style(40.0, 50.0), FormattingContextType::Block, vec![]);
  item1.id = 1;

  let mut item2 = BoxNode::new_block(item_style(40.0, 50.0), FormattingContextType::Block, vec![]);
  item2.id = 2;

  let mut item3 = BoxNode::new_block(item_style(40.0, 30.0), FormattingContextType::Block, vec![]);
  item3.id = 3;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item1, item2, item3],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  // Wrapping behavior should be identical to `wrap` for sizing purposes: line 1 is 50px tall, line
  // 2 is 30px tall, with a 5px row gap between them.
  let expected_height = 50.0 + 5.0 + 30.0;
  let eps = 1e-3;
  assert!(
    (fragment.bounds.height() - expected_height).abs() < eps,
    "expected flex-wrap:wrap-reverse auto height to include all lines; got {}",
    fragment.bounds.height()
  );
}
