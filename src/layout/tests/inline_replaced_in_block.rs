use crate::style::display::Display;
use crate::style::types::LineHeight;
use crate::style::values::Length;
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::{CrossOriginAttribute, ImageDecodingAttribute, ReplacedType};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{
  BoxNode, BoxTree, ComputedStyle, FontConfig, FormattingContextType, LayoutConfig, LayoutEngine,
  Size,
};
use std::sync::Arc;

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  node.children.iter().find_map(find_first_line)
}

fn collect_replaced<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_replaced(child, out);
  }
}

fn collect_replaced_abs_y<'a>(
  node: &'a FragmentNode,
  parent_y: f32,
  out: &mut Vec<(f32, &'a FragmentNode)>,
) {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    out.push((abs_y, node));
  }
  for child in node.children.iter() {
    collect_replaced_abs_y(child, abs_y, out);
  }
}

#[test]
fn inline_replaced_children_form_single_line_in_block_context() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;

  let img1 = BoxNode::new_replaced(
    Arc::new(inline_style.clone()),
    ReplacedType::Image {
      src: "a.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(10.0, 10.0)),
    Some(1.0),
  );

  let img2 = BoxNode::new_replaced(
    Arc::new(inline_style),
    ReplacedType::Image {
      src: "b.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(20.0, 10.0)),
    Some(2.0),
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![img1, img2],
  );
  let tree = BoxTree::new(root);

  let engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(200.0, 100.0)));
  let fragments = engine.layout_tree(&tree).expect("layout");

  let line = find_first_line(&fragments.root).expect("expected a line fragment");
  let mut replaced = Vec::new();
  collect_replaced(line, &mut replaced);
  assert_eq!(
    replaced.len(),
    2,
    "expected both replaced elements to participate in the same inline formatting context"
  );

  let first = replaced[0].bounds;
  let second = replaced[1].bounds;
  assert!(
    (first.y() - second.y()).abs() < 0.01,
    "expected replaced elements to share a line: first.y={} second.y={}",
    first.y(),
    second.y()
  );
  assert!(
    second.x() > first.x() + 0.01,
    "expected second replaced element to be positioned after the first: first.x={} second.x={}",
    first.x(),
    second.x()
  );
}

#[test]
fn inline_replaced_line_boxes_include_strut_descent() {
  // Regression test: inline replaced elements (like `<img>`) are baseline-aligned and should still
  // reserve the line box's descender/leading. This is the common "inline images have a small gap
  // underneath" behavior seen in browsers.
  //
  // Without including the strut, a line containing only a baseline-aligned image collapses to the
  // image height, so a subsequent `<br>`-separated image starts immediately after it with no extra
  // gap.
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.font_size = 20.0;
  root_style.line_height = LineHeight::Length(Length::px(40.0));

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;

  let img1 = BoxNode::new_replaced(
    Arc::new(inline_style.clone()),
    ReplacedType::Image {
      src: "a.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(80.0, 80.0)),
    Some(1.0),
  );

  let br = BoxNode::new_line_break(Arc::new(inline_style.clone()));

  let img2 = BoxNode::new_replaced(
    Arc::new(inline_style),
    ReplacedType::Image {
      src: "b.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(80.0, 80.0)),
    Some(1.0),
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![img1, br, img2],
  );
  let tree = BoxTree::new(root);

  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let engine = LayoutEngine::with_font_context(
    LayoutConfig::for_viewport(Size::new(200.0, 400.0)),
    font_context,
  );
  let fragments = engine.layout_tree(&tree).expect("layout");

  let mut replaced = Vec::new();
  collect_replaced_abs_y(&fragments.root, 0.0, &mut replaced);
  replaced.sort_by(|a, b| a.0.total_cmp(&b.0));
  assert_eq!(replaced.len(), 2, "expected exactly two replaced fragments");
  let (y1, f1) = replaced[0];
  let (y2, _f2) = replaced[1];

  let h1 = f1.bounds.height();
  assert!(
    y2 > y1 + h1 + 0.5,
    "expected a baseline/strut gap below the first inline image line: y1={y1} h1={h1} y2={y2}"
  );
}
