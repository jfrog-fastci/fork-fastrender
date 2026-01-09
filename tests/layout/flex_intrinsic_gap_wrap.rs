use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use fastrender::FragmentNode;
use std::sync::Arc;

fn find_block_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| {
      panic!(
        "missing fragment for box_id={box_id}; got children ids={:?}",
        fragment.children.iter().map(|c| c.box_id()).collect::<Vec<_>>()
      )
    })
}

#[test]
fn flex_wrap_uses_max_content_including_gap() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(195.0));
  container_style.width_keyword = None;
  container_style.grid_row_gap = Length::px(0.0);
  container_style.grid_column_gap = Length::px(0.0);

  let mut fixed_style = ComputedStyle::default();
  fixed_style.display = Display::Block;
  fixed_style.width = Some(Length::px(90.0));
  fixed_style.height = Some(Length::px(10.0));
  fixed_style.width_keyword = None;
  fixed_style.height_keyword = None;
  fixed_style.flex_shrink = 0.0;

  let mut fixed = BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);
  fixed.id = 1;

  // A nested flex container whose max-content width depends on its `column-gap`.
  let mut nested_style = ComputedStyle::default();
  nested_style.display = Display::Flex;
  nested_style.flex_wrap = FlexWrap::NoWrap;
  nested_style.grid_row_gap = Length::px(0.0);
  nested_style.grid_column_gap = Length::px(10.0);
  nested_style.flex_shrink = 0.0;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(50.0));
  item_style.height = Some(Length::px(10.0));
  item_style.width_keyword = None;
  item_style.height_keyword = None;
  item_style.flex_shrink = 0.0;
  let item_style = Arc::new(item_style);

  let mut nested_child_1 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
  nested_child_1.id = 3;
  let mut nested_child_2 = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
  nested_child_2.id = 4;

  let mut nested = BoxNode::new_block(
    Arc::new(nested_style),
    FormattingContextType::Flex,
    vec![nested_child_1, nested_child_2],
  );
  nested.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed, nested],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(195.0))
    .expect("layout succeeds");

  let nested_fragment = find_block_child(&fragment, 2);
  assert!(
    nested_fragment.bounds.y() > 0.0,
    "expected nested flex item to wrap onto a new line; got y={}",
    nested_fragment.bounds.y()
  );
  assert!(
    (nested_fragment.bounds.y() - 10.0).abs() <= 0.5,
    "expected nested flex item to start at y≈10 (line height); got y={}",
    nested_fragment.bounds.y()
  );
}

