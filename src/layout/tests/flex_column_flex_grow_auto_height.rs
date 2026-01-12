use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::{AlignItems, FlexDirection};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::Size;
use std::sync::Arc;

#[test]
fn flex_column_percent_width_auto_min_height_uses_definite_container_width() {
  // Regression test for dropbox.com hero:
  // - A column flex container uses `align-items:center` (so items are *not* stretched).
  // - A flex item uses `width:100%` (percentage, resolved against the container's inner width).
  // - The item contains a percentage padding box (`padding-top:100%`), so its block size depends on
  //   the used width.
  //
  // Flexbox automatic minimum size (`min-height:auto`) must compute the content-based minimum height
  // using the *definite* container cross size so percentage widths/padding resolve correctly.
  //
  // Buggy behaviour: the content-based minimum height was computed via intrinsic block-size probes
  // with no percentage base, falling back to the viewport width and forcing a viewport-width-tall
  // square.
  let fc = FlexFormattingContext::with_viewport(Size::new(800.0, 600.0));

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.align_items = AlignItems::Center;
  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![],
  );
  container.id = 1;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::percent(100.0));
  item_style.width_keyword = None;
  let mut item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  item.id = 2;

  // An empty block box whose height is established by percentage padding (relative to its width).
  let mut square_style = ComputedStyle::default();
  square_style.display = Display::Block;
  square_style.width = Some(Length::percent(100.0));
  square_style.width_keyword = None;
  square_style.padding_top = Length::percent(100.0);
  let mut square = BoxNode::new_block(Arc::new(square_style), FormattingContextType::Block, vec![]);
  square.id = 3;

  item.children.push(square);
  container.children.push(item);

  let container_width = 300.0;
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(container_width),
    AvailableSpace::Indefinite,
  );

  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout succeeds");
  let item_fragment = fragment.children.first().expect("item fragment");
  let height = item_fragment.bounds.height();

  assert!(
    (height - container_width).abs() < 0.5,
    "expected percent-based padding box to produce height={container_width}, got {height}"
  );
}
