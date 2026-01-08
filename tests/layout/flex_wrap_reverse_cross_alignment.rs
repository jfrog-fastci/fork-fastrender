use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::WritingMode;
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
fn flex_wrap_reverse_single_line_align_items_end_uses_block_end() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::End;
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
    "short child should align to block-end (bottom) within the line: {:?}",
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

#[test]
fn flex_wrap_reverse_multi_line_align_content_end_packs_lines_to_bottom() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::End;
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
    "first line (tall child) should be at the bottom under wrap-reverse + align-content:end: {:?}",
    debug_children
  );
  assert!(
    (short_y - 70.0).abs() < 1e-3,
    "second line (short child) should stack above the first line under wrap-reverse + align-content:end: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_rl_align_content_start_packs_lines_to_block_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::Start;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 80.0).abs() < 1e-3,
    "first column should start at x=80 under vertical-rl + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 90.0).abs() < 1e-3,
    "second column should start at x=90 under vertical-rl + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_content_start_packs_lines_to_block_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::Start;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 10.0).abs() < 1e-3,
    "first column should start at x=10 under vertical-lr + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 0.0).abs() < 1e-3,
    "second column should start at x=0 under vertical-lr + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_rl_align_content_flex_start_packs_lines_to_cross_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 0.0).abs() < 1e-3,
    "first column should start at x=0 under vertical-rl + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 10.0).abs() < 1e-3,
    "second column should start at x=10 under vertical-rl + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_content_flex_start_packs_lines_to_cross_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 90.0).abs() < 1e-3,
    "first column should start at x=90 under vertical-lr + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 80.0).abs() < 1e-3,
    "second column should start at x=80 under vertical-lr + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_content_end_packs_lines_to_block_end() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::End;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 90.0).abs() < 1e-3,
    "first column should start at x=90 under vertical-lr + wrap-reverse + align-content:end: {:?}",
    debug_children
  );
  assert!(
    (last_x - 80.0).abs() < 1e-3,
    "second column should start at x=80 under vertical-lr + wrap-reverse + align-content:end: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_content_flex_end_packs_lines_to_cross_end() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexEnd;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    child.id = id;
    children.push(child);
  }

  let first_id = children[0].id;
  let last_id = children[2].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  let mut first_x = None;
  let mut last_x = None;
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
      Some(id) if id == first_id => first_x = Some(child.bounds.x()),
      Some(id) if id == last_id => last_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = first_x.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_x = last_x.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  assert!(
    (first_x - 10.0).abs() < 1e-3,
    "first column should start at x=10 under vertical-lr + wrap-reverse + align-content:flex-end: {:?}",
    debug_children
  );
  assert!(
    (last_x - 0.0).abs() < 1e-3,
    "second column should start at x=0 under vertical-lr + wrap-reverse + align-content:flex-end: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_rl_align_items_start_uses_block_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 0.0).abs() < 1e-3,
    "wide child should start at the line's left edge (fills line width): {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 10.0).abs() < 1e-3,
    "narrow child should align to the line's block-start edge (right) under vertical-rl: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_items_start_uses_block_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 80.0).abs() < 1e-3,
    "wide child should start at the line's left edge (fills line width): {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 80.0).abs() < 1e-3,
    "narrow child should align to the line's block-start edge (left) under vertical-lr: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_items_end_uses_block_end_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::End;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 80.0).abs() < 1e-3,
    "wide child should start at the line's left edge (fills line width): {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 90.0).abs() < 1e-3,
    "narrow child should align to the line's block-end edge (right) under vertical-lr: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_rl_align_items_flex_start_uses_cross_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 0.0).abs() < 1e-3,
    "wide child should start at the line's left edge: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 0.0).abs() < 1e-3,
    "narrow child should align to the flex cross-start edge (left) under vertical-rl + wrap-reverse: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_items_flex_start_uses_cross_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 80.0).abs() < 1e-3,
    "wide child should start at the line's left edge: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 90.0).abs() < 1e-3,
    "narrow child should align to the flex cross-start edge (right) under vertical-lr + wrap-reverse: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_vertical_lr_align_items_flex_end_uses_cross_end_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(10.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let mut wide_child =
    BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
  wide_child.id = 1;

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(10.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let mut narrow_child =
    BoxNode::new_block(Arc::new(narrow_style), FormattingContextType::Block, vec![]);
  narrow_child.id = 2;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let mut wide_x = None;
  let mut narrow_x = None;
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
      Some(id) if id == wide_id => wide_x = Some(child.bounds.x()),
      Some(id) if id == narrow_id => narrow_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let wide_x = wide_x.unwrap_or_else(|| panic!("missing wide child: {:?}", debug_children));
  let narrow_x = narrow_x.unwrap_or_else(|| panic!("missing narrow child: {:?}", debug_children));

  assert!(
    (wide_x - 80.0).abs() < 1e-3,
    "wide child should start at the line's left edge: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 80.0).abs() < 1e-3,
    "narrow child should align to the flex cross-end edge (left) under vertical-lr + wrap-reverse: {:?}",
    debug_children
  );
}
