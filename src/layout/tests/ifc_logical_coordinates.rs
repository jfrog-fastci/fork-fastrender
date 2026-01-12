use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::inline::InlineFormattingContext;
use crate::style::display::Display;
use crate::style::types::WritingMode;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::tree::box_tree::ImageDecodingAttribute;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SrcsetCandidate;
use crate::tree::fragment_tree::FragmentContent;
use crate::{Point, Rect, Size};
use std::sync::Arc;

fn find_abs_bounds_by_box_id(root: &crate::FragmentNode, box_id: usize) -> Option<Rect> {
  let mut stack = vec![(root, Point::ZERO)];
  while let Some((node, parent_origin)) = stack.pop() {
    let abs_origin = Point::new(
      parent_origin.x + node.bounds.origin.x,
      parent_origin.y + node.bounds.origin.y,
    );
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline {
        box_id: Some(id), ..
      }
      | FragmentContent::Text {
        box_id: Some(id), ..
      }
      | FragmentContent::Replaced {
        box_id: Some(id), ..
      } => *id == box_id,
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
  let mut style = crate::ComputedStyle::default();
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
fn inline_fc_layout_with_floats_returns_logical_coordinates_in_vertical_writing_modes() {
  // `InlineFormattingContext::layout_with_floats` is used by block layout as an internal helper.
  // It should always produce fragments in the parent's *logical* coordinate system where:
  //   x = inline axis, y = block axis.
  //
  // For `writing-mode: vertical-rl`, the inline axis runs vertically, so the second inline item
  // should advance along `x` (logical inline) rather than `y` (logical block).
  let writing_mode = WritingMode::VerticalRl;

  let img1 = image_node(1, Size::new(20.0, 20.0), writing_mode);
  let img2 = image_node(2, Size::new(20.0, 20.0), writing_mode);

  let mut root_style = crate::ComputedStyle::default();
  root_style.display = Display::Inline;
  root_style.writing_mode = writing_mode;
  let mut root = BoxNode::new_inline(Arc::new(root_style), vec![img1, img2]);
  root.id = 3;

  // In vertical writing modes, the inline axis corresponds to the physical height, so give it
  // enough height for both images to land on the same line.
  let constraints = LayoutConstraints::definite(100.0, 200.0);
  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout_with_floats(&root, &constraints, None, 0.0, 0.0)
    .expect("layout with floats");

  let img1_bounds = find_abs_bounds_by_box_id(&fragment, 1).expect("img1 fragment");
  let img2_bounds = find_abs_bounds_by_box_id(&fragment, 2).expect("img2 fragment");

  assert!(
    img2_bounds.x() > img1_bounds.x() + 10.0,
    "expected inline progression to advance along logical x (img1.x={}, img2.x={})",
    img1_bounds.x(),
    img2_bounds.x()
  );
  assert!(
    (img2_bounds.y() - img1_bounds.y()).abs() < 0.1,
    "expected both images to share the same logical block position (img1.y={}, img2.y={})",
    img1_bounds.y(),
    img2_bounds.y()
  );
}
