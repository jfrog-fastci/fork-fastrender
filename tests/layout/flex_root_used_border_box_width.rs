use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn max_descendant_right(fragment: &fastrender::tree::fragment_tree::FragmentNode) -> f32 {
  fragment
    .iter_fragments()
    .map(|f| f.bounds.max_x())
    .fold(0.0, f32::max)
}

#[test]
fn flex_root_uses_definite_used_border_box_width_over_available_width() {
  // Regression for LA Times-style negative margin "gutter" rows:
  //
  // The block formatting context can resolve a flex container’s used border-box width to be larger
  // than the containing block width (e.g. negative horizontal margins). It passes that in via
  // `used_border_box_width` while `available_width` remains the containing block size (used for
  // percentage bases).
  //
  // Flex layout must not clamp the root fragment back down to `available_width`, or flex items can
  // overflow the container and end up misaligned.
  let fc = FlexFormattingContext::with_viewport(Size::new(1200.0, 800.0));

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.margin_left = Some(Length::px(-20.0));
  container_style.margin_right = Some(Length::px(-20.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::percent(71.0));

  let mut right_style = ComputedStyle::default();
  right_style.display = Display::Block;
  right_style.width = Some(Length::percent(29.0));
  right_style.min_width = Some(Length::px(290.0));

  let left = BoxNode::new_block(Arc::new(left_style), FormattingContextType::Block, Vec::new());
  let right = BoxNode::new_block(Arc::new(right_style), FormattingContextType::Block, Vec::new());

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, right],
  );

  // The containing block is 840px wide, but the block width:auto resolution (taking negative
  // margins into account) yields a 880px used border-box size.
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(840.0),
    AvailableSpace::Indefinite,
  )
  .with_inline_percentage_base(Some(840.0))
  .with_used_border_box_size(Some(880.0), None);

  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.width() - 880.0).abs() < 0.5,
    "flex root should respect used_border_box_width (got {:.1})",
    fragment.bounds.width()
  );

  let max_right = max_descendant_right(&fragment);
  assert!(
    max_right <= fragment.bounds.width() + 0.5,
    "flex items should not overflow the root bounds (max_right={:.1}, root_width={:.1})",
    max_right,
    fragment.bounds.width()
  );
}

