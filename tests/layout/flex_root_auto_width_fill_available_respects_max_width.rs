use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_root_auto_width_fill_available_respects_max_width() {
  let fc = FlexFormattingContext::with_viewport(Size::new(800.0, 600.0));

  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.max_width = Some(Length::px(150.0));

  let container = BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, Vec::new());

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(300.0))
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.width() - 150.0).abs() < 0.5,
    "expected max-width to clamp fill-available width:auto (got {:.1})",
    fragment.bounds.width()
  );
}

