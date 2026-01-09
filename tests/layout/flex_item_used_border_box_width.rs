use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BoxSizing;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn calc(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn flex_item_used_border_box_width_drives_block_child_percentage_resolution() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(500.0));

  let mut sidebar_style = ComputedStyle::default();
  sidebar_style.display = Display::Block;
  sidebar_style.width = Some(Length::px(100.0));
  sidebar_style.flex_grow = 0.0;
  sidebar_style.flex_shrink = 0.0;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.width = Some(calc(100.0, -50.0));
  inner_style.height = Some(Length::px(10.0));

  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut content_style = ComputedStyle::default();
  content_style.display = Display::Block;
  content_style.width = Some(calc(100.0, -100.0));
  content_style.box_sizing = BoxSizing::BorderBox;
  content_style.padding_left = Length::px(20.0);
  content_style.padding_right = Length::px(20.0);
  content_style.flex_grow = 0.0;
  content_style.flex_shrink = 0.0;

  let content =
    BoxNode::new_block(Arc::new(content_style), FormattingContextType::Block, vec![inner]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![
      BoxNode::new_block(Arc::new(sidebar_style), FormattingContextType::Block, vec![]),
      content,
    ],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let content_frag = fragment.children.get(1).expect("content fragment");
  assert!(
    (content_frag.bounds.width() - 400.0).abs() < 0.1,
    "expected content border-box width 400px, got {}",
    content_frag.bounds.width()
  );

  let inner_frag = content_frag.children.first().expect("inner fragment");
  assert!(
    (inner_frag.bounds.width() - 310.0).abs() < 0.1,
    "expected inner border-box width 310px, got {}",
    inner_frag.bounds.width()
  );
}
