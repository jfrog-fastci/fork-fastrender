use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::CrossOriginAttribute;
use fastrender::tree::box_tree::ImageDecodingAttribute;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::SrcsetCandidate;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::Point;
use fastrender::Rect;
use fastrender::Size;
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

fn find_abs_bounds_by_box_id(fragment: &FragmentNode, box_id: usize) -> Option<Rect> {
  fn recurse(node: &FragmentNode, box_id: usize, origin: Point) -> Option<Rect> {
    let abs_bounds = node.bounds.translate(origin);
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(abs_bounds);
    }
    let child_origin = origin.translate(node.bounds.origin);
    for child in node.children_ref() {
      if let Some(found) = recurse(child, box_id, child_origin) {
        return Some(found);
      }
    }
    None
  }

  recurse(fragment, box_id, Point::ZERO)
}

#[test]
fn abspos_descendant_inside_inline_wrapper_uses_positioned_ancestor_containing_block() {
  // Regression test for absolute positioning inside inline content.
  //
  // Pattern:
  //   positioned block (definite size)
  //     inline wrapper (e.g. <a>)
  //       abspos "fill" child (e.g. <img style="position:absolute; inset:0">)
  //
  // The abspos child is out-of-flow and its containing block is the nearest positioned ancestor
  // (CSS 2.1 §10.1). It must *not* accidentally use the anonymous inline container that block
  // layout creates for laying out runs of inline-level children, which typically has the height
  // of a single line box (~line-height).
  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(400.0));

  let mut positioned_style = ComputedStyle::default();
  positioned_style.position = Position::Relative;
  positioned_style.width = Some(Length::px(200.0));
  positioned_style.height = Some(Length::px(100.0));

  let mut link_style = ComputedStyle::default();
  link_style.position = Position::Static;

  let mut img_style = ComputedStyle::default();
  img_style.position = Position::Absolute;
  img_style.width = Some(Length::percent(100.0));
  img_style.top = InsetValue::Length(Length::px(0.0));
  img_style.right = InsetValue::Length(Length::px(0.0));
  img_style.bottom = InsetValue::Length(Length::px(0.0));
  img_style.left = InsetValue::Length(Length::px(0.0));

  let mut img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Image {
      src: String::new(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::<SrcsetCandidate>::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(50.0, 20.0)),
    None,
  );
  img.id = 3;

  let mut link = BoxNode::new_inline(Arc::new(link_style), vec![img]);
  link.id = 2;

  let mut positioned =
    BoxNode::new_block(Arc::new(positioned_style), FormattingContextType::Block, vec![link]);
  positioned.id = 1;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![positioned],
  );

  let constraints = LayoutConstraints::definite(400.0, 300.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let img_fragment = find_fragment_by_box_id(&fragment, 3).expect("image fragment should exist");
  assert!(
    (img_fragment.bounds.width() - 200.0).abs() < 0.1,
    "expected abspos child to fill positioned ancestor width (got {})",
    img_fragment.bounds.width()
  );
  assert!(
    (img_fragment.bounds.height() - 100.0).abs() < 0.1,
    "expected abspos child to fill positioned ancestor height (got {})",
    img_fragment.bounds.height()
  );
}

#[test]
fn abspos_inset_offsets_resolve_against_padding_edge_in_block_layout() {
  // Regression test for absolute positioning when the containing block is established by a
  // positioned block with padding, and the abspos element is nested under inline content.
  //
  // In particular, when block layout produces child fragments in the *content* coordinate space
  // (0,0 at the content edge) and later translates them into border-box coordinates, the containing
  // block's padding box must also be represented in content coordinates. Otherwise, abspos
  // descendants end up shifted by the parent's padding.
  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(300.0));
  root_style.height = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height_keyword = None;

  let mut positioned_style = ComputedStyle::default();
  positioned_style.position = Position::Relative;
  positioned_style.width = Some(Length::px(200.0));
  positioned_style.height = Some(Length::px(100.0));
  positioned_style.width_keyword = None;
  positioned_style.height_keyword = None;
  positioned_style.padding_left = Length::px(15.0);
  positioned_style.padding_top = Length::px(7.0);

  let mut link_style = ComputedStyle::default();
  link_style.position = Position::Static;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.width_keyword = None;
  abs_style.height_keyword = None;

  let mut abs_child = BoxNode::new_inline(Arc::new(abs_style), vec![]);
  abs_child.id = 3;

  let mut link = BoxNode::new_inline(Arc::new(link_style), vec![abs_child]);
  link.id = 2;

  let mut positioned =
    BoxNode::new_block(Arc::new(positioned_style), FormattingContextType::Block, vec![link]);
  positioned.id = 1;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![positioned],
  );

  let constraints = LayoutConstraints::definite(300.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let positioned_bounds =
    find_abs_bounds_by_box_id(&fragment, 1).expect("positioned fragment should exist");
  let abs_bounds = find_abs_bounds_by_box_id(&fragment, 3).expect("abs fragment should exist");

  assert!(
    (abs_bounds.x() - positioned_bounds.x()).abs() < 0.1,
    "expected abspos left offset to resolve against padding edge (got abs_x={} positioned_x={})",
    abs_bounds.x(),
    positioned_bounds.x()
  );
  assert!(
    (abs_bounds.y() - positioned_bounds.y()).abs() < 0.1,
    "expected abspos top offset to resolve against padding edge (got abs_y={} positioned_y={})",
    abs_bounds.y(),
    positioned_bounds.y()
  );
}

#[test]
fn abspos_direct_child_resolves_insets_against_padding_edge_in_block_layout() {
  // Regression test for `position:absolute` direct children of a padded positioned block.
  //
  // This matches real-world patterns like the IETF homepage hero overlay:
  //   .jumbotron { padding: 4rem 2rem; position: relative; }
  //   .jumbotron::before { position: absolute; inset: 0; }
  //
  // The abspos child should start at the padding edge, not at (-padding_left, -padding_top).
  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(400.0));
  root_style.height = Some(Length::px(400.0));
  root_style.width_keyword = None;
  root_style.height_keyword = None;

  let mut positioned_style = ComputedStyle::default();
  positioned_style.position = Position::Relative;
  positioned_style.width = Some(Length::px(200.0));
  positioned_style.height = Some(Length::px(100.0));
  positioned_style.width_keyword = None;
  positioned_style.height_keyword = None;
  positioned_style.padding_left = Length::px(32.0);
  positioned_style.padding_top = Length::px(64.0);
  positioned_style.padding_right = Length::px(32.0);
  positioned_style.padding_bottom = Length::px(64.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.width_keyword = None;
  abs_style.height_keyword = None;

  let mut abs_child = BoxNode::new_inline(Arc::new(abs_style), vec![]);
  abs_child.id = 2;

  let mut flow_child_style = ComputedStyle::default();
  flow_child_style.height = Some(Length::px(1.0));
  flow_child_style.height_keyword = None;
  let mut flow_child = BoxNode::new_block(
    Arc::new(flow_child_style),
    FormattingContextType::Block,
    vec![],
  );
  flow_child.id = 3;

  let mut positioned = BoxNode::new_block(
    Arc::new(positioned_style),
    FormattingContextType::Block,
    vec![abs_child, flow_child],
  );
  positioned.id = 1;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![positioned],
  );

  let constraints = LayoutConstraints::definite(400.0, 400.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let positioned_bounds =
    find_abs_bounds_by_box_id(&fragment, 1).expect("positioned fragment should exist");
  let abs_bounds = find_abs_bounds_by_box_id(&fragment, 2).expect("abs fragment should exist");

  assert!(
    (abs_bounds.x() - positioned_bounds.x()).abs() < 0.1,
    "expected abspos left offset to resolve against padding edge (got abs_x={} positioned_x={})",
    abs_bounds.x(),
    positioned_bounds.x()
  );
  assert!(
    (abs_bounds.y() - positioned_bounds.y()).abs() < 0.1,
    "expected abspos top offset to resolve against padding edge (got abs_y={} positioned_y={})",
    abs_bounds.y(),
    positioned_bounds.y()
  );
}
