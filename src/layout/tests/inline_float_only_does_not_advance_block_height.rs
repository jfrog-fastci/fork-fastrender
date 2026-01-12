use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::style::display::Display;
use crate::style::float::Float;
use crate::style::values::Length;
use crate::tree::box_tree::{CrossOriginAttribute, ImageDecodingAttribute, ReplacedType};
use crate::tree::fragment_tree::FragmentNode;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType, Size};
use std::sync::Arc;

fn collect_by_size<'a>(
  node: &'a FragmentNode,
  target_width: f32,
  target_height: f32,
  out: &mut Vec<&'a FragmentNode>,
) {
  if (node.bounds.width() - target_width).abs() < 0.01
    && (node.bounds.height() - target_height).abs() < 0.01
  {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_by_size(child, target_width, target_height, out);
  }
}

#[test]
fn inline_float_only_anonymous_block_does_not_advance_in_flow_height() {
  // When an anonymous block run contains only floats (e.g. `<a><img style=float:left></a>`),
  // those floats must not advance the in-flow cursor. Otherwise subsequent floats in the same
  // block formatting context are forced downward.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut anon_style = ComputedStyle::default();
  anon_style.display = Display::Block;

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Inline;
  img_style.float = Float::Left;

  let img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Image {
      src: "float.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(50.0, 20.0)),
    Some(2.5),
  );

  let inline_wrapper = BoxNode::new_inline(Arc::new(inline_style), vec![img]);
  let float_only_run = BoxNode::new_anonymous_block(Arc::new(anon_style), vec![inline_wrapper]);

  let mut float_right_style = ComputedStyle::default();
  float_right_style.display = Display::Block;
  float_right_style.float = Float::Right;
  float_right_style.width = Some(Length::px(33.0));
  float_right_style.height = Some(Length::px(11.0));
  let float_right = BoxNode::new_block(
    Arc::new(float_right_style),
    FormattingContextType::Block,
    vec![],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![float_only_run, float_right],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let mut float_right_frags = Vec::new();
  collect_by_size(&fragment, 33.0, 11.0, &mut float_right_frags);
  assert_eq!(
    float_right_frags.len(),
    1,
    "expected one right float fragment"
  );

  let frag = float_right_frags[0];
  assert!(
    frag.bounds.y().abs() < 0.01,
    "expected right float to be eligible to start at y=0, got y={:.2}",
    frag.bounds.y()
  );
  assert!(
    (frag.bounds.x() - 167.0).abs() < 0.01,
    "expected right float to be positioned against the right edge, got x={:.2}",
    frag.bounds.x()
  );
}
