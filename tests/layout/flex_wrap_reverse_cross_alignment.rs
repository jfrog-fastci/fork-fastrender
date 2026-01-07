use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn fragment_box_id(content: &FragmentContent) -> Option<usize> {
  match content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

#[test]
fn flex_wrap_reverse_single_line_cross_start_alignment_flips() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.width = Some(Length::px(10.0));
  tall_style.width_keyword = None;
  tall_style.height = Some(Length::px(20.0));
  tall_style.height_keyword = None;
  tall_style.flex_shrink = 0.0;
  let mut tall_child =
    BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);
  tall_child.id = 1;

  let mut short_style = ComputedStyle::default();
  short_style.display = Display::Block;
  short_style.width = Some(Length::px(10.0));
  short_style.width_keyword = None;
  short_style.height = Some(Length::px(10.0));
  short_style.height_keyword = None;
  short_style.flex_shrink = 0.0;
  let mut short_child =
    BoxNode::new_block(Arc::new(short_style), FormattingContextType::Block, vec![]);
  short_child.id = 2;

  let tall_id = tall_child.id;
  let short_id = short_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![tall_child, short_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 100.0))
    .expect("layout succeeds");

  let mut tall_y = None;
  let mut short_y = None;
  let mut debug_children = Vec::new();

  for child in fragment.children.iter() {
    let id = fragment_box_id(&child.content);
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == tall_id => tall_y = Some(child.bounds.y()),
      Some(id) if id == short_id => short_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let tall_y = tall_y.unwrap_or_else(|| panic!("missing tall child: {:?}", debug_children));
  let short_y = short_y.unwrap_or_else(|| panic!("missing short child: {:?}", debug_children));

  assert!(
    (tall_y - 80.0).abs() < 1e-3,
    "tall child should be pushed to the bottom under wrap-reverse: {:?}",
    debug_children
  );
  assert!(
    (short_y - 90.0).abs() < 1e-3,
    "short child should align to cross-start (bottom) within the line: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_single_line_align_items_start_uses_block_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.width = Some(Length::px(10.0));
  tall_style.width_keyword = None;
  tall_style.height = Some(Length::px(20.0));
  tall_style.height_keyword = None;
  tall_style.flex_shrink = 0.0;
  let mut tall_child =
    BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);
  tall_child.id = 1;

  let mut short_style = ComputedStyle::default();
  short_style.display = Display::Block;
  short_style.width = Some(Length::px(10.0));
  short_style.width_keyword = None;
  short_style.height = Some(Length::px(10.0));
  short_style.height_keyword = None;
  short_style.flex_shrink = 0.0;
  let mut short_child =
    BoxNode::new_block(Arc::new(short_style), FormattingContextType::Block, vec![]);
  short_child.id = 2;

  let tall_id = tall_child.id;
  let short_id = short_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![tall_child, short_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 100.0))
    .expect("layout succeeds");

  let mut tall_y = None;
  let mut short_y = None;
  let mut debug_children = Vec::new();

  for child in fragment.children.iter() {
    let id = fragment_box_id(&child.content);
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == tall_id => tall_y = Some(child.bounds.y()),
      Some(id) if id == short_id => short_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let tall_y = tall_y.unwrap_or_else(|| panic!("missing tall child: {:?}", debug_children));
  let short_y = short_y.unwrap_or_else(|| panic!("missing short child: {:?}", debug_children));

  assert!(
    (tall_y - 80.0).abs() < 1e-3,
    "tall child should align to the line's block-start (top edge) under wrap-reverse: {:?}",
    debug_children
  );
  assert!(
    (short_y - 80.0).abs() < 1e-3,
    "short child should align to the line's block-start (top edge) under wrap-reverse: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_multi_line_align_content_packs_lines_to_bottom() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(15.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.width = Some(Length::px(10.0));
  tall_style.width_keyword = None;
  tall_style.height = Some(Length::px(20.0));
  tall_style.height_keyword = None;
  tall_style.flex_shrink = 0.0;
  let mut tall_child =
    BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);
  tall_child.id = 1;

  let mut short_style = ComputedStyle::default();
  short_style.display = Display::Block;
  short_style.width = Some(Length::px(10.0));
  short_style.width_keyword = None;
  short_style.height = Some(Length::px(10.0));
  short_style.height_keyword = None;
  short_style.flex_shrink = 0.0;
  let mut short_child =
    BoxNode::new_block(Arc::new(short_style), FormattingContextType::Block, vec![]);
  short_child.id = 2;

  let tall_id = tall_child.id;
  let short_id = short_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![tall_child, short_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(15.0, 100.0))
    .expect("layout succeeds");

  let mut tall_y = None;
  let mut short_y = None;
  let mut debug_children = Vec::new();

  for child in fragment.children.iter() {
    let id = fragment_box_id(&child.content);
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == tall_id => tall_y = Some(child.bounds.y()),
      Some(id) if id == short_id => short_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let tall_y = tall_y.unwrap_or_else(|| panic!("missing tall child: {:?}", debug_children));
  let short_y = short_y.unwrap_or_else(|| panic!("missing short child: {:?}", debug_children));

  assert!(
    (tall_y - 80.0).abs() < 1e-3,
    "first line (tall child) should be at the bottom under wrap-reverse: {:?}",
    debug_children
  );
  assert!(
    (short_y - 70.0).abs() < 1e-3,
    "second line (short child) should stack above the first line under wrap-reverse: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_multi_line_align_content_start_packs_lines_to_top() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  // `start` aligns to block-start (top) and should differ from `flex-start` under wrap-reverse.
  container_style.align_content = AlignContent::Start;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(15.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.width = Some(Length::px(10.0));
  tall_style.width_keyword = None;
  tall_style.height = Some(Length::px(20.0));
  tall_style.height_keyword = None;
  tall_style.flex_shrink = 0.0;
  let mut tall_child =
    BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);
  tall_child.id = 1;

  let mut short_style = ComputedStyle::default();
  short_style.display = Display::Block;
  short_style.width = Some(Length::px(10.0));
  short_style.width_keyword = None;
  short_style.height = Some(Length::px(10.0));
  short_style.height_keyword = None;
  short_style.flex_shrink = 0.0;
  let mut short_child =
    BoxNode::new_block(Arc::new(short_style), FormattingContextType::Block, vec![]);
  short_child.id = 2;

  let tall_id = tall_child.id;
  let short_id = short_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![tall_child, short_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(15.0, 100.0))
    .expect("layout succeeds");

  let mut tall_y = None;
  let mut short_y = None;
  let mut debug_children = Vec::new();

  for child in fragment.children.iter() {
    let id = fragment_box_id(&child.content);
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == tall_id => tall_y = Some(child.bounds.y()),
      Some(id) if id == short_id => short_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let tall_y = tall_y.unwrap_or_else(|| panic!("missing tall child: {:?}", debug_children));
  let short_y = short_y.unwrap_or_else(|| panic!("missing short child: {:?}", debug_children));

  assert!(
    (short_y - 0.0).abs() < 1e-3,
    "top line (short child) should align to block-start under wrap-reverse + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (tall_y - 10.0).abs() < 1e-3,
    "bottom line (tall child) should stack below the top line under wrap-reverse + align-content:start: {:?}",
    debug_children
  );
}
