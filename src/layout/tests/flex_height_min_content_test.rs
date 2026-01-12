use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::BoxSizing;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::LineHeight;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_height_min_content_behaves_like_auto() {
  // Repro for linkedin.com: CTA-style buttons use `height: min-content` + `min-height` + padding.
  // FastRender previously resolved `height:min-content` by doing an intrinsic block-size probe that
  // forced a min-content inline-size layout, causing extra wrapping and a much taller used height.
  //
  // Spec (CSS Sizing L3): min-content/max-content in the block axis behave like `auto` for normal
  // boxes, so `height: min-content` should not cause wrapping-driven inflation.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(500.0));
  container_style.overflow_x = Overflow::Visible;
  container_style.overflow_y = Overflow::Visible;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.box_sizing = BoxSizing::BorderBox;
  item_style.height_keyword = Some(IntrinsicSizeKeyword::MinContent);
  item_style.min_height = Some(Length::px(48.0));
  item_style.padding_top = Length::px(12.0);
  item_style.padding_bottom = Length::px(12.0);
  item_style.padding_left = Length::px(24.0);
  item_style.padding_right = Length::px(24.0);
  item_style.font_size = 16.0;
  item_style.line_height = LineHeight::Number(1.5);
  item_style.overflow_x = Overflow::Visible;
  item_style.overflow_y = Overflow::Visible;

  let mut text_style = ComputedStyle::default();
  text_style.font_size = 16.0;
  text_style.line_height = LineHeight::Number(1.5);

  let text = BoxNode::new_text(Arc::new(text_style), "Create an account".to_string());
  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![text],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let height = child.bounds.height();
  assert!(
    (height - 48.0).abs() < 0.75,
    "expected `height:min-content` to behave like auto (clamped by min-height) with one line of text; got {height}"
  );
}
