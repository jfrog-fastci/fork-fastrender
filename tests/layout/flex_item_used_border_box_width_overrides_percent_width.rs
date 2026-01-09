use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_used_border_box_width_overrides_percent_width_for_descendant_layout() {
  // Regression for LA Times: when a flex/grid parent has already resolved an item's used inline
  // size, the item may still have an authored percentage `width`. Block layout must not re-resolve
  // that percentage against the item's own used size (which would effectively apply the
  // percentage twice), otherwise descendants end up laid out too narrow.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(500.0));

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::percent(50.0));

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.width = Some(Length::percent(100.0));
  inner_style.height = Some(Length::px(10.0));

  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, Vec::new());
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![inner]);

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

  let item_fragment = fragment.children.first().expect("flex item fragment");
  assert!(
    (item_fragment.bounds.width() - 250.0).abs() < 0.5,
    "expected flex item to size to 50% of 500px (got {:.1})",
    item_fragment.bounds.width()
  );

  let inner_fragment = item_fragment
    .children
    .first()
    .expect("descendant block fragment");
  assert!(
    (inner_fragment.bounds.width() - 250.0).abs() < 0.5,
    "descendant layout should use the flex-resolved used width (got {:.1})",
    inner_fragment.bounds.width()
  );
}

