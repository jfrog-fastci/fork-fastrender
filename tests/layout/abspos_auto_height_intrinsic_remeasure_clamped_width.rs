use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::FlexDirection;
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

#[test]
fn abspos_auto_height_remeasures_intrinsic_height_at_clamped_width() {
  // Regression test: non-replaced abspos boxes with `height:auto` should use their intrinsic/content
  // height measured at the *used* width.
  //
  // Discord's `.home_clyde` is:
  //   position:absolute; bottom:0; width:100%; max-width:2.5rem; height:auto; display:flex;
  // and contains a `width:100%` replaced image. The image's used height depends on the container's
  // resolved width after `max-width` clamping.
  let mut container_style = ComputedStyle::default();
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Flex;
  abs_style.flex_direction = FlexDirection::Column;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::percent(100.0));
  abs_style.max_width = Some(Length::px(40.0));
  abs_style.height = None; // auto

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Block;
  img_style.width = Some(Length::percent(100.0));
  img_style.height = None; // auto

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
    Some(Size::new(139.0, 156.0)),
    None,
  );
  img.id = 3;

  let mut abs_child =
    BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Flex, vec![img]);
  abs_child.id = 2;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );
  container.id = 1;

  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![container],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("layout");

  let abs_fragment = find_fragment_by_box_id(&fragment, 2).expect("abspos flex fragment should exist");
  let img_fragment = find_fragment_by_box_id(&fragment, 3).expect("image fragment should exist");

  let expected_height = 40.0 * (156.0 / 139.0);
  assert!(
    (abs_fragment.bounds.width() - 40.0).abs() < 0.1,
    "expected max-width to clamp abspos container width (got {})",
    abs_fragment.bounds.width()
  );
  assert!(
    (img_fragment.bounds.width() - 40.0).abs() < 0.1,
    "expected image width to match clamped container width (got {})",
    img_fragment.bounds.width()
  );
  assert!(
    (img_fragment.bounds.height() - expected_height).abs() < 0.5,
    "expected image height to be derived from clamped width via intrinsic ratio (got {})",
    img_fragment.bounds.height()
  );
  assert!(
    (abs_fragment.bounds.height() - expected_height).abs() < 0.5,
    "expected container height to shrink-wrap image at clamped width (got {})",
    abs_fragment.bounds.height()
  );
  assert!(
    (abs_fragment.bounds.y() - (200.0 - expected_height)).abs() < 0.5,
    "expected bottom alignment using clamped intrinsic height (got y={}, h={})",
    abs_fragment.bounds.y(),
    abs_fragment.bounds.height()
  );
}
