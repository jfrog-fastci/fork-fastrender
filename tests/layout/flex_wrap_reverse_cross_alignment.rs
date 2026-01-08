use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::Direction;
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

fn layout_wrap_reverse_overflow_alignment(align_content: AlignContent) -> (f32, f32, f32) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = align_content;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(60.0));
  container_style.width_keyword = None;
  // Two 10px-tall lines (20px total) in a 15px container => negative free space.
  container_style.height = Some(Length::px(15.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(30.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(10.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;
  let child_style = Arc::new(child_style);

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
    child.id = id;
    children.push(child);
  }

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 15.0))
    .expect("layout succeeds");

  let mut child1_y = None;
  let mut child2_y = None;
  let mut child3_y = None;
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
      Some(1) => child1_y = Some(child.bounds.y()),
      Some(2) => child2_y = Some(child.bounds.y()),
      Some(3) => child3_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let child1_y = child1_y.unwrap_or_else(|| panic!("missing child1: {:?}", debug_children));
  let child2_y = child2_y.unwrap_or_else(|| panic!("missing child2: {:?}", debug_children));
  let child3_y = child3_y.unwrap_or_else(|| panic!("missing child3: {:?}", debug_children));

  (child1_y, child2_y, child3_y)
}

fn layout_wrap_reverse_overflow_alignment_vertical_rl(
  align_content: AlignContent,
) -> (f32, f32, f32) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = align_content;
  container_style.align_items = AlignItems::FlexStart;
  // Two 10px-wide columns (20px total) in a 15px container => negative free space.
  container_style.width = Some(Length::px(15.0));
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
  let child_style = Arc::new(child_style);

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
    child.id = id;
    children.push(child);
  }

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(15.0, 40.0))
    .expect("layout succeeds");

  let mut child1_x = None;
  let mut child2_x = None;
  let mut child3_x = None;
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
      Some(1) => child1_x = Some(child.bounds.x()),
      Some(2) => child2_x = Some(child.bounds.x()),
      Some(3) => child3_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let child1_x = child1_x.unwrap_or_else(|| panic!("missing child1: {:?}", debug_children));
  let child2_x = child2_x.unwrap_or_else(|| panic!("missing child2: {:?}", debug_children));
  let child3_x = child3_x.unwrap_or_else(|| panic!("missing child3: {:?}", debug_children));

  (child1_x, child2_x, child3_x)
}

fn layout_wrap_reverse_overflow_alignment_rtl_column(
  align_content: AlignContent,
) -> (f32, f32, f32) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = align_content;
  container_style.align_items = AlignItems::FlexStart;
  // Two 10px-wide columns (20px total) in a 15px container => negative free space.
  container_style.width = Some(Length::px(15.0));
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
  let child_style = Arc::new(child_style);

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
    child.id = id;
    children.push(child);
  }

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(15.0, 40.0))
    .expect("layout succeeds");

  let mut child1_x = None;
  let mut child2_x = None;
  let mut child3_x = None;
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
      Some(1) => child1_x = Some(child.bounds.x()),
      Some(2) => child2_x = Some(child.bounds.x()),
      Some(3) => child3_x = Some(child.bounds.x()),
      _ => {}
    }
  }

  let child1_x = child1_x.unwrap_or_else(|| panic!("missing child1: {:?}", debug_children));
  let child2_x = child2_x.unwrap_or_else(|| panic!("missing child2: {:?}", debug_children));
  let child3_x = child3_x.unwrap_or_else(|| panic!("missing child3: {:?}", debug_children));

  (child1_x, child2_x, child3_x)
}

fn layout_wrap_reverse_vertical_writing_mode_rtl_column_align_content(
  writing_mode: WritingMode,
  align_content: AlignContent,
) -> (f32, f32) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = writing_mode;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = align_content;
  container_style.align_items = AlignItems::FlexStart;
  container_style.width = Some(Length::px(20.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(100.0));
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height = Some(Length::px(20.0));
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;
  let child_style = Arc::new(child_style);

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
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
    .layout(&container, &LayoutConstraints::definite(20.0, 100.0))
    .expect("layout succeeds");

  let mut first_y = None;
  let mut last_y = None;
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
      Some(id) if id == first_id => first_y = Some(child.bounds.y()),
      Some(id) if id == last_id => last_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let first_y = first_y.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
  let last_y = last_y.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

  (first_y, last_y)
}

fn layout_wrap_reverse_vertical_writing_mode_rtl_column_align_items(
  writing_mode: WritingMode,
  align_items: AlignItems,
) -> (f32, f32) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = writing_mode;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = align_items;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(20.0));
  container_style.height_keyword = None;

  let mut tall_style = ComputedStyle::default();
  tall_style.display = Display::Block;
  tall_style.width = Some(Length::px(10.0));
  tall_style.width_keyword = None;
  tall_style.height = Some(Length::px(20.0));
  tall_style.height_keyword = None;
  tall_style.flex_shrink = 0.0;
  let mut tall_child = BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);
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
    .layout(&container, &LayoutConstraints::definite(100.0, 20.0))
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

  (tall_y, short_y)
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
fn flex_wrap_reverse_rtl_column_align_content_start_packs_lines_to_inline_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
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
    "first column should start at x=80 under rtl + column + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 90.0).abs() < 1e-3,
    "second column should start at x=90 under rtl + column + wrap-reverse + align-content:start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_rtl_column_align_content_flex_start_packs_lines_to_cross_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
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
    "first column should start at x=0 under rtl + column + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 10.0).abs() < 1e-3,
    "second column should start at x=10 under rtl + column + wrap-reverse + align-content:flex-start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_rtl_column_align_items_start_uses_inline_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
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
  let mut wide_child = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
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
    (narrow_x - 10.0).abs() < 1e-3,
    "narrow child should align to inline-start (right) within the line under rtl + wrap-reverse cancellation: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_reverse_rtl_column_align_items_flex_start_uses_cross_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
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
  let mut wide_child = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
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
    "narrow child should align to cross-start (left) within the line under rtl + wrap-reverse cancellation: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_rtl_column_align_content_start_packs_lines_to_inline_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::Wrap;
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
    (first_x - 90.0).abs() < 1e-3,
    "first column should start at x=90 under rtl + column + wrap + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 80.0).abs() < 1e-3,
    "second column should start at x=80 under rtl + column + wrap + align-content:start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_rtl_column_align_content_end_packs_lines_to_inline_end() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::Wrap;
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
    (first_x - 10.0).abs() < 1e-3,
    "first column should start at x=10 under rtl + column + wrap + align-content:end: {:?}",
    debug_children
  );
  assert!(
    (last_x - 0.0).abs() < 1e-3,
    "second column should start at x=0 under rtl + column + wrap + align-content:end: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_rtl_column_align_items_start_uses_inline_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::Wrap;
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
  let mut wide_child = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
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
    "wide child should align to inline-start (right) within the line under rtl + column + wrap: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 90.0).abs() < 1e-3,
    "narrow child should align to inline-start (right) within the line under rtl + column + wrap: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_rtl_column_align_items_end_uses_inline_end_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::Wrap;
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
  let mut wide_child = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);
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
    "wide child should align to inline-end (left) within the line under rtl + column + wrap: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 80.0).abs() < 1e-3,
    "narrow child should align to inline-end (left) within the line under rtl + column + wrap: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_vertical_writing_mode_rtl_column_align_content_start_packs_lines_to_inline_start() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.writing_mode = writing_mode;
    container_style.direction = Direction::Rtl;
    container_style.flex_direction = FlexDirection::Column;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.align_content = AlignContent::Start;
    container_style.align_items = AlignItems::FlexStart;
    container_style.width = Some(Length::px(20.0));
    container_style.width_keyword = None;
    container_style.height = Some(Length::px(100.0));
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
      .layout(&container, &LayoutConstraints::definite(20.0, 100.0))
      .expect("layout succeeds");

    let mut first_y = None;
    let mut last_y = None;
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
        Some(id) if id == first_id => first_y = Some(child.bounds.y()),
        Some(id) if id == last_id => last_y = Some(child.bounds.y()),
        _ => {}
      }
    }

    let first_y = first_y.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
    let last_y = last_y.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (0.0, 20.0)
    } else {
      (80.0, 60.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap + align-content:start: {:?}",
      debug_children
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap + align-content:start: {:?}",
      debug_children
    );
  }
}

#[test]
fn flex_wrap_vertical_writing_mode_rtl_column_align_content_end_packs_lines_to_inline_end() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.writing_mode = writing_mode;
    container_style.direction = Direction::Rtl;
    container_style.flex_direction = FlexDirection::Column;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.align_content = AlignContent::End;
    container_style.align_items = AlignItems::FlexStart;
    container_style.width = Some(Length::px(20.0));
    container_style.width_keyword = None;
    container_style.height = Some(Length::px(100.0));
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
      .layout(&container, &LayoutConstraints::definite(20.0, 100.0))
      .expect("layout succeeds");

    let mut first_y = None;
    let mut last_y = None;
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
        Some(id) if id == first_id => first_y = Some(child.bounds.y()),
        Some(id) if id == last_id => last_y = Some(child.bounds.y()),
        _ => {}
      }
    }

    let first_y = first_y.unwrap_or_else(|| panic!("missing first child: {:?}", debug_children));
    let last_y = last_y.unwrap_or_else(|| panic!("missing last child: {:?}", debug_children));

    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (60.0, 80.0)
    } else {
      (20.0, 0.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap + align-content:end: {:?}",
      debug_children
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap + align-content:end: {:?}",
      debug_children
    );
  }
}

#[test]
fn flex_wrap_vertical_writing_mode_rtl_column_align_items_start_uses_inline_start_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.writing_mode = writing_mode;
    container_style.direction = Direction::Rtl;
    container_style.flex_direction = FlexDirection::Column;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.align_content = AlignContent::FlexStart;
    container_style.align_items = AlignItems::Start;
    container_style.width = Some(Length::px(100.0));
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
      .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
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

    let (expected_tall_y, expected_short_y) = if writing_mode == WritingMode::SidewaysLr {
      (0.0, 0.0)
    } else {
      (80.0, 90.0)
    };
    assert!(
      (tall_y - expected_tall_y).abs() < 1e-3,
      "tall child should start at y={expected_tall_y} under {writing_mode:?} + rtl + column + wrap: {:?}",
      debug_children
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to inline-start under {writing_mode:?} + rtl + column + wrap: {:?}",
      debug_children
    );
  }
}

#[test]
fn flex_wrap_vertical_writing_mode_rtl_column_align_items_end_uses_inline_end_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.writing_mode = writing_mode;
    container_style.direction = Direction::Rtl;
    container_style.flex_direction = FlexDirection::Column;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.align_content = AlignContent::FlexStart;
    container_style.align_items = AlignItems::End;
    container_style.width = Some(Length::px(100.0));
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
      .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
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

    let (expected_tall_y, expected_short_y) = if writing_mode == WritingMode::SidewaysLr {
      (0.0, 10.0)
    } else {
      (80.0, 80.0)
    };
    assert!(
      (tall_y - expected_tall_y).abs() < 1e-3,
      "tall child should start at y={expected_tall_y} under {writing_mode:?} + rtl + column + wrap: {:?}",
      debug_children
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to inline-end under {writing_mode:?} + rtl + column + wrap: {:?}",
      debug_children
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_content_start_packs_lines_to_inline_start() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (first_y, last_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_content(
      writing_mode,
      AlignContent::Start,
    );
    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (20.0, 0.0)
    } else {
      (60.0, 80.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:start; got y={first_y}"
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:start; got y={last_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_content_flex_start_packs_lines_to_cross_start() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (first_y, last_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_content(
      writing_mode,
      AlignContent::FlexStart,
    );
    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (80.0, 60.0)
    } else {
      (0.0, 20.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:flex-start; got y={first_y}"
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:flex-start; got y={last_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_content_end_packs_lines_to_inline_end() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (first_y, last_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_content(
      writing_mode,
      AlignContent::End,
    );
    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (80.0, 60.0)
    } else {
      (0.0, 20.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:end; got y={first_y}"
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:end; got y={last_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_content_flex_end_packs_lines_to_cross_end() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (first_y, last_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_content(
      writing_mode,
      AlignContent::FlexEnd,
    );
    let (expected_first_y, expected_last_y) = if writing_mode == WritingMode::SidewaysLr {
      (20.0, 0.0)
    } else {
      (60.0, 80.0)
    };
    assert!(
      (first_y - expected_first_y).abs() < 1e-3,
      "first line should start at y={expected_first_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:flex-end; got y={first_y}"
    );
    assert!(
      (last_y - expected_last_y).abs() < 1e-3,
      "second line should start at y={expected_last_y} under {writing_mode:?} + rtl + column + wrap-reverse + align-content:flex-end; got y={last_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_items_start_uses_inline_start_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (tall_y, short_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_items(
      writing_mode,
      AlignItems::Start,
    );
    let expected_short_y = if writing_mode == WritingMode::SidewaysLr {
      0.0
    } else {
      10.0
    };
    assert!(
      (tall_y - 0.0).abs() < 1e-3,
      "tall child should start at y=0 under {writing_mode:?} + rtl + column + wrap-reverse; got y={tall_y}"
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to inline-start under {writing_mode:?} + rtl + column + wrap-reverse; got y={short_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_items_flex_start_uses_cross_start_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (tall_y, short_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_items(
      writing_mode,
      AlignItems::FlexStart,
    );
    let expected_short_y = if writing_mode == WritingMode::SidewaysLr {
      10.0
    } else {
      0.0
    };
    assert!(
      (tall_y - 0.0).abs() < 1e-3,
      "tall child should start at y=0 under {writing_mode:?} + rtl + column + wrap-reverse; got y={tall_y}"
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to cross-start under {writing_mode:?} + rtl + column + wrap-reverse; got y={short_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_items_end_uses_inline_end_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (tall_y, short_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_items(
      writing_mode,
      AlignItems::End,
    );
    let expected_short_y = if writing_mode == WritingMode::SidewaysLr {
      10.0
    } else {
      0.0
    };
    assert!(
      (tall_y - 0.0).abs() < 1e-3,
      "tall child should start at y=0 under {writing_mode:?} + rtl + column + wrap-reverse; got y={tall_y}"
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to inline-end under {writing_mode:?} + rtl + column + wrap-reverse; got y={short_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_writing_mode_rtl_column_align_items_flex_end_uses_cross_end_within_line() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (tall_y, short_y) = layout_wrap_reverse_vertical_writing_mode_rtl_column_align_items(
      writing_mode,
      AlignItems::FlexEnd,
    );
    let expected_short_y = if writing_mode == WritingMode::SidewaysLr {
      0.0
    } else {
      10.0
    };
    assert!(
      (tall_y - 0.0).abs() < 1e-3,
      "tall child should start at y=0 under {writing_mode:?} + rtl + column + wrap-reverse; got y={tall_y}"
    );
    assert!(
      (short_y - expected_short_y).abs() < 1e-3,
      "short child should align to cross-end under {writing_mode:?} + rtl + column + wrap-reverse; got y={short_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_align_content_distributed_overflow_falls_back_to_safe_start() {
  // Distributed alignment values fall back to a "safe" alignment when there is no free space. Under
  // wrap-reverse, the cross axis is mirrored, so ensure that safe start alignment survives the
  // mirroring step.
  let eps = 1e-3;
  for align in [
    AlignContent::SpaceBetween,
    AlignContent::SpaceAround,
    AlignContent::SpaceEvenly,
    AlignContent::Stretch,
  ] {
    let (child1_y, child2_y, child3_y) = layout_wrap_reverse_overflow_alignment(align);
    assert!(
      (child3_y - 0.0).abs() < eps,
      "align-content={align:?}: expected last line (child3) to pack to y=0 under safe start fallback; got y={child3_y}"
    );
    assert!(
      (child1_y - 10.0).abs() < eps,
      "align-content={align:?}: expected first line (child1) to start at y=10 under wrap-reverse; got y={child1_y}"
    );
    assert!(
      (child2_y - 10.0).abs() < eps,
      "align-content={align:?}: expected first line (child2) to start at y=10 under wrap-reverse; got y={child2_y}"
    );
  }
}

#[test]
fn flex_wrap_reverse_vertical_rl_align_content_distributed_overflow_falls_back_to_safe_start() {
  // When the physical cross-start edge is the right edge (`writing-mode: vertical-rl`) and we don't
  // mirror the cross axis for wrap-reverse, ensure the "safe start" fallback for distributed
  // align-content values still packs the line stack to the physical start edge.
  let eps = 1e-3;
  for align in [
    AlignContent::SpaceBetween,
    AlignContent::SpaceAround,
    AlignContent::SpaceEvenly,
    AlignContent::Stretch,
  ] {
    let (child1_x, child2_x, child3_x) = layout_wrap_reverse_overflow_alignment_vertical_rl(align);
    assert!(
      (child3_x - 5.0).abs() < eps,
      "align-content={align:?}: expected second column (child3) to align to x=5 under safe start fallback; got x={child3_x}"
    );
    assert!(
      (child1_x + 5.0).abs() < eps,
      "align-content={align:?}: expected first column (child1) to overflow to x=-5 under safe start fallback; got x={child1_x}"
    );
    assert!(
      (child2_x + 5.0).abs() < eps,
      "align-content={align:?}: expected first column (child2) to overflow to x=-5 under safe start fallback; got x={child2_x}"
    );
  }
}

#[test]
fn flex_wrap_reverse_rtl_column_align_content_distributed_overflow_falls_back_to_safe_start() {
  // Same as above but with a horizontal inline axis in RTL mode. Inline-start is the physical right
  // edge, and wrap-reverse flips cross-start/cross-end, so ensure the safe fallback still aligns to
  // the physical start edge.
  let eps = 1e-3;
  for align in [
    AlignContent::SpaceBetween,
    AlignContent::SpaceAround,
    AlignContent::SpaceEvenly,
    AlignContent::Stretch,
  ] {
    let (child1_x, child2_x, child3_x) = layout_wrap_reverse_overflow_alignment_rtl_column(align);
    assert!(
      (child3_x - 5.0).abs() < eps,
      "align-content={align:?}: expected second column (child3) to align to x=5 under safe start fallback; got x={child3_x}"
    );
    assert!(
      (child1_x + 5.0).abs() < eps,
      "align-content={align:?}: expected first column (child1) to overflow to x=-5 under safe start fallback; got x={child1_x}"
    );
    assert!(
      (child2_x + 5.0).abs() < eps,
      "align-content={align:?}: expected first column (child2) to overflow to x=-5 under safe start fallback; got x={child2_x}"
    );
  }
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
fn flex_wrap_vertical_rl_align_content_start_packs_lines_to_block_start() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
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
    (first_x - 90.0).abs() < 1e-3,
    "first column should start at x=90 under vertical-rl + wrap + align-content:start: {:?}",
    debug_children
  );
  assert!(
    (last_x - 80.0).abs() < 1e-3,
    "second column should start at x=80 under vertical-rl + wrap + align-content:start: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_vertical_rl_align_content_end_packs_lines_to_block_end() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
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
    (first_x - 10.0).abs() < 1e-3,
    "first column should start at x=10 under vertical-rl + wrap + align-content:end: {:?}",
    debug_children
  );
  assert!(
    (last_x - 0.0).abs() < 1e-3,
    "second column should start at x=0 under vertical-rl + wrap + align-content:end: {:?}",
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
fn flex_wrap_vertical_rl_align_items_start_uses_block_start_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(20.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let wide_style = Arc::new(wide_style);

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(20.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let narrow_style = Arc::new(narrow_style);

  let mut filler_style = ComputedStyle::default();
  filler_style.display = Display::Block;
  filler_style.width = Some(Length::px(20.0));
  filler_style.width_keyword = None;
  filler_style.height = Some(Length::px(20.0));
  filler_style.height_keyword = None;
  filler_style.flex_shrink = 0.0;
  let filler_style = Arc::new(filler_style);

  let mut wide_child = BoxNode::new_block(wide_style, FormattingContextType::Block, vec![]);
  wide_child.id = 1;
  let mut narrow_child = BoxNode::new_block(narrow_style, FormattingContextType::Block, vec![]);
  narrow_child.id = 2;
  let mut filler_child = BoxNode::new_block(filler_style, FormattingContextType::Block, vec![]);
  filler_child.id = 3;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child, filler_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
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
    "wide child should start at the line's left edge under vertical-rl + wrap: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 90.0).abs() < 1e-3,
    "narrow child should align to the line's block-start edge (right) under vertical-rl + wrap: {:?}",
    debug_children
  );
}

#[test]
fn flex_wrap_vertical_rl_align_items_end_uses_block_end_within_line() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::End;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height = Some(Length::px(40.0));
  container_style.height_keyword = None;

  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.width = Some(Length::px(20.0));
  wide_style.width_keyword = None;
  wide_style.height = Some(Length::px(20.0));
  wide_style.height_keyword = None;
  wide_style.flex_shrink = 0.0;
  let wide_style = Arc::new(wide_style);

  let mut narrow_style = ComputedStyle::default();
  narrow_style.display = Display::Block;
  narrow_style.width = Some(Length::px(10.0));
  narrow_style.width_keyword = None;
  narrow_style.height = Some(Length::px(20.0));
  narrow_style.height_keyword = None;
  narrow_style.flex_shrink = 0.0;
  let narrow_style = Arc::new(narrow_style);

  let mut filler_style = ComputedStyle::default();
  filler_style.display = Display::Block;
  filler_style.width = Some(Length::px(20.0));
  filler_style.width_keyword = None;
  filler_style.height = Some(Length::px(20.0));
  filler_style.height_keyword = None;
  filler_style.flex_shrink = 0.0;
  let filler_style = Arc::new(filler_style);

  let mut wide_child = BoxNode::new_block(wide_style, FormattingContextType::Block, vec![]);
  wide_child.id = 1;
  let mut narrow_child = BoxNode::new_block(narrow_style, FormattingContextType::Block, vec![]);
  narrow_child.id = 2;
  let mut filler_child = BoxNode::new_block(filler_style, FormattingContextType::Block, vec![]);
  filler_child.id = 3;

  let wide_id = wide_child.id;
  let narrow_id = narrow_child.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wide_child, narrow_child, filler_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 40.0))
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
    "wide child should start at the line's left edge under vertical-rl + wrap: {:?}",
    debug_children
  );
  assert!(
    (narrow_x - 80.0).abs() < 1e-3,
    "narrow child should align to the line's block-end edge (left) under vertical-rl + wrap: {:?}",
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
