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
fn abspos_replaced_max_width_clamp_preserves_intrinsic_ratio() {
  // Regression test: absolutely positioned replaced elements should preserve their intrinsic ratio
  // when `max-width` clamps the resolved width while `height` is `auto`.
  //
  // This is required for patterns like:
  //   img { position: absolute; max-width: 100%; height: auto; }
  // which are common in real-world fixtures (e.g. Ars Technica cards).
  let mut container_style = ComputedStyle::default();
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(500.0));
  container_style.height = Some(Length::px(400.0));

  let mut img_style = ComputedStyle::default();
  img_style.position = Position::Absolute;
  img_style.left = InsetValue::Length(Length::px(0.0));
  img_style.top = InsetValue::Length(Length::px(0.0));
  img_style.width = None; // auto
  img_style.height = None; // auto
  img_style.max_width = Some(Length::percent(100.0));

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
    Some(Size::new(768.0, 432.0)),
    None,
  );
  img.id = 2;

  let mut container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Block, vec![img]);
  container.id = 1;

  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![container],
  );

  let constraints = LayoutConstraints::definite(500.0, 400.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("layout");

  let img_fragment = find_fragment_by_box_id(&fragment, 2).expect("image fragment should exist");
  assert!(
    (img_fragment.bounds.width() - 500.0).abs() < 0.1,
    "expected max-width to clamp abspos image to container width (got {})",
    img_fragment.bounds.width()
  );
  assert!(
    (img_fragment.bounds.height() - 281.25).abs() < 0.5,
    "expected auto height to be derived from the clamped width via intrinsic ratio (got {})",
    img_fragment.bounds.height()
  );
}

