use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_width_calc_percentage_resolves_against_container_inner_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let calc = |percent: f32, px: f32| -> Length {
    let calc = CalcLength::single(LengthUnit::Percent, percent)
      .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
      .expect("calc expression should be representable");
    Length::calc(calc)
  };

  let mut sidebar_style = ComputedStyle::default();
  sidebar_style.display = Display::Block;
  sidebar_style.width = Some(Length::px(50.0));
  sidebar_style.flex_grow = 0.0;
  sidebar_style.flex_shrink = 0.0;

  let mut content_style = ComputedStyle::default();
  content_style.display = Display::Block;
  content_style.width = Some(calc(100.0, -50.0));
  content_style.padding_left = Length::px(10.0);
  content_style.padding_right = Length::px(10.0);
  content_style.border_left_width = Length::px(5.0);
  content_style.border_right_width = Length::px(5.0);
  content_style.flex_grow = 0.0;
  content_style.flex_shrink = 1.0;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![
      BoxNode::new_block(Arc::new(sidebar_style), FormattingContextType::Block, vec![]),
      BoxNode::new_block(Arc::new(content_style), FormattingContextType::Block, vec![]),
    ],
  );

  let fc = FlexFormattingContext::new();
  let container_width = 300.0;
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(container_width), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let sidebar = fragment.children.get(0).expect("sidebar fragment");
  let content = fragment.children.get(1).expect("content fragment");

  assert!(
    (sidebar.bounds.width() - 50.0).abs() < 0.1,
    "expected sidebar width 50px, got {}",
    sidebar.bounds.width()
  );

  assert!(
    (content.bounds.width() - 250.0).abs() < 0.1,
    "expected content border-box width 250px, got {}",
    content.bounds.width()
  );
}
