use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, FlexDirection, FlexWrap};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};

fn find_block_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| {
      panic!(
        "missing fragment for box_id={box_id}; got children ids={:?}",
        fragment
          .children
          .iter()
          .map(|c| c.box_id())
          .collect::<Vec<_>>()
      )
    })
}

#[test]
fn flex_multiline_auto_height_nested_container_size() {
  // Regression test for flex containers used as flex items: the nested container's `height:auto`
  // should expand to fit all wrapped lines (including row gaps).
  let fc = FlexFormattingContext::new();

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Row;
  outer_style.align_items = AlignItems::FlexStart;
  outer_style.width = Some(Length::px(200.0));
  outer_style.width_keyword = None;

  let mut nested_style = ComputedStyle::default();
  nested_style.display = Display::Flex;
  nested_style.flex_direction = FlexDirection::Row;
  nested_style.flex_wrap = FlexWrap::Wrap;
  nested_style.grid_column_gap = Length::px(10.0);
  nested_style.grid_row_gap = Length::px(5.0);
  nested_style.width = Some(Length::px(100.0));
  nested_style.width_keyword = None;

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

  // 5 items -> 3 lines (2 + 2 + 1) at 100px width with a 10px column gap.
  let mut item1 = BoxNode::new_block(item_style(40.0, 50.0), FormattingContextType::Block, vec![]);
  item1.id = 11;
  let mut item2 = BoxNode::new_block(item_style(40.0, 10.0), FormattingContextType::Block, vec![]);
  item2.id = 12;
  let mut item3 = BoxNode::new_block(item_style(40.0, 40.0), FormattingContextType::Block, vec![]);
  item3.id = 13;
  let mut item4 = BoxNode::new_block(item_style(40.0, 5.0), FormattingContextType::Block, vec![]);
  item4.id = 14;
  let mut item5 = BoxNode::new_block(item_style(40.0, 30.0), FormattingContextType::Block, vec![]);
  item5.id = 15;

  let mut nested = BoxNode::new_block(
    Arc::new(nested_style),
    FormattingContextType::Flex,
    vec![item1, item2, item3, item4, item5],
  );
  nested.id = 2;

  // Add a sibling to ensure the nested container participates in sizing/alignment as a flex item.
  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.width = Some(Length::px(20.0));
  sibling_style.height = Some(Length::px(10.0));
  sibling_style.width_keyword = None;
  sibling_style.height_keyword = None;
  let mut sibling = BoxNode::new_block(
    Arc::new(sibling_style),
    FormattingContextType::Block,
    vec![],
  );
  sibling.id = 3;

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![nested, sibling],
  );

  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let nested_fragment = find_block_child(&fragment, 2);

  let expected_height = 50.0 + 5.0 + 40.0 + 5.0 + 30.0;
  let eps = 1e-3;
  assert!(
    (nested_fragment.bounds.height() - expected_height).abs() < eps,
    "expected nested flex container auto height to include all wrapped lines, got {}",
    nested_fragment.bounds.height()
  );
}
