use fastrender::style::display::Display;
use fastrender::tree::box_tree::{CrossOriginAttribute, ReplacedType};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, BoxTree, ComputedStyle, FormattingContextType, LayoutConfig, LayoutEngine, Size};
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

