use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use fastrender::FragmentNode;
use std::sync::Arc;

fn find_first_fragment_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id().is_some_and(|fragment_id| fragment_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn assert_approx(value: f32, expected: f32, msg: &str) {
  assert!(
    (value - expected).abs() <= 0.5,
    "{msg}: got {value:.2} expected {expected:.2}"
  );
}

#[test]
fn flex_zero_sized_items_do_not_expand_to_container_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  let container_style = Arc::new(container_style);

  let mut empty_style = ComputedStyle::default();
  empty_style.display = Display::Block;
  let empty_style = Arc::new(empty_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(50.0));
  let item_style = Arc::new(item_style);

  let mut empty = BoxNode::new_block(empty_style, FormattingContextType::Block, vec![]);
  empty.id = 1;
  let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
  item.id = 2;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![empty, item],
  );
  container.id = 3;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  let empty_fragment = find_first_fragment_with_id(&fragment, 1).expect("empty fragment");
  let item_fragment = find_first_fragment_with_id(&fragment, 2).expect("item fragment");

  assert_approx(empty_fragment.bounds.width(), 0.0, "empty item should remain 0px wide");
  assert_approx(item_fragment.bounds.x(), 0.0, "next item should not be pushed");
  assert_approx(item_fragment.bounds.width(), 50.0, "item should keep its preferred width");
}
