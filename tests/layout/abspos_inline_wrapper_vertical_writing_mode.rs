use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{BorderStyle, InsetValue, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::CrossOriginAttribute;
use fastrender::tree::box_tree::ImageDecodingAttribute;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::SrcsetCandidate;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{Point, Rect, Size};
use std::sync::Arc;

fn find_abs_bounds_by_box_id(root: &fastrender::FragmentNode, box_id: usize) -> Option<Rect> {
  let mut stack = vec![(root, Point::ZERO)];
  while let Some((node, parent_origin)) = stack.pop() {
    let abs_origin = Point::new(parent_origin.x + node.bounds.origin.x, parent_origin.y + node.bounds.origin.y);
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(Rect::new(abs_origin, node.bounds.size));
    }
    for child in node.children.iter() {
      stack.push((child, abs_origin));
    }
  }
  None
}

fn image_node(id: usize, size: Size, writing_mode: WritingMode) -> BoxNode {
  let mut style = fastrender::ComputedStyle::default();
  style.display = Display::Inline;
  style.writing_mode = writing_mode;

  let mut node = BoxNode::new_replaced(
    Arc::new(style),
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
    Some(size),
    None,
  );
  node.id = id;
  node
}

#[test]
fn abspos_descendant_inside_positioned_inline_wrapper_in_vertical_writing_mode_uses_wrapper_padding_box() {
  // Regression coverage for the historical coordinate-space mismatch between block/inline layout
  // when `writing-mode` is vertical (block axis is horizontal).
  //
  // Pattern:
  //   vertical writing-mode block container
  //     positioned inline wrapper (<span style="position:relative">)
  //       in-flow replaced content (gives the wrapper a non-zero size)
  //       abspos child (inset:0) should fill wrapper padding box (not the outer block)
  let writing_mode = WritingMode::VerticalRl;

  let mut root_style = fastrender::ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.writing_mode = writing_mode;
  root_style.width = Some(Length::px(200.0));
  root_style.height = Some(Length::px(150.0));

  // The inline wrapper establishes the containing block.
  let mut wrapper_style = fastrender::ComputedStyle::default();
  wrapper_style.display = Display::Inline;
  wrapper_style.writing_mode = writing_mode;
  wrapper_style.position = Position::Relative;
  wrapper_style.border_left_style = BorderStyle::Solid;
  wrapper_style.border_right_style = BorderStyle::Solid;
  wrapper_style.border_top_style = BorderStyle::Solid;
  wrapper_style.border_bottom_style = BorderStyle::Solid;
  wrapper_style.border_left_width = Length::px(3.0);
  wrapper_style.border_right_width = Length::px(7.0);
  wrapper_style.border_top_width = Length::px(5.0);
  wrapper_style.border_bottom_width = Length::px(11.0);

  // In-flow content so the wrapper has a predictable border box size.
  let img = image_node(3, Size::new(60.0, 30.0), writing_mode);

  // Absolutely positioned empty block that should fill the wrapper's padding box.
  let mut abs_style = fastrender::ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.writing_mode = writing_mode;
  abs_style.position = Position::Absolute;
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 4;

  let mut wrapper = BoxNode::new_inline(Arc::new(wrapper_style), vec![img, abs_child]);
  wrapper.id = 2;

  let mut root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![wrapper],
  );
  root.id = 1;

  let constraints = LayoutConstraints::definite(400.0, 400.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let wrapper_bounds = find_abs_bounds_by_box_id(&fragment, 2).expect("wrapper fragment");
  let abs_bounds = find_abs_bounds_by_box_id(&fragment, 4).expect("abspos fragment");

  let border_left = 3.0;
  let border_right = 7.0;
  let border_top = 5.0;
  let border_bottom = 11.0;
  let expected = Rect::from_xywh(
    wrapper_bounds.x() + border_left,
    wrapper_bounds.y() + border_top,
    wrapper_bounds.width() - border_left - border_right,
    wrapper_bounds.height() - border_top - border_bottom,
  );

  assert!(
    (abs_bounds.x() - expected.x()).abs() < 0.1
      && (abs_bounds.y() - expected.y()).abs() < 0.1
      && (abs_bounds.width() - expected.width()).abs() < 0.1
      && (abs_bounds.height() - expected.height()).abs() < 0.1,
    "expected abspos child to fill wrapper padding box in physical coordinates; wrapper={:?} expected={:?} got={:?}",
    wrapper_bounds,
    expected,
    abs_bounds
  );
}

