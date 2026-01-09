use fastrender::dom::{DomNode, DomNodeType};
use fastrender::interaction::{hit_test_dom, resolve_label_associated_control, HitTestKind};
use fastrender::style::types::PointerEvents;
use fastrender::{BoxNode, BoxTree, ComputedStyle, FragmentNode, FragmentTree, Point, Rect};
use selectors::context::QuirksMode;
use std::sync::Arc;

fn doc(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children,
  }
}

fn elem(tag: &str, attrs: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: String::new(),
      attributes: attrs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    },
    children,
  }
}

fn text(content: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Text {
      content: content.to_string(),
    },
    children: Vec::new(),
  }
}

#[test]
fn hit_test_dom_resolves_link_ancestor() {
  let dom = doc(vec![elem(
    "a",
    vec![("id", "link"), ("href", "/foo")],
    vec![elem("span", vec![], vec![text("txt")])],
  )]);

  // DOM ids (pre-order):
  // 1 document
  // 2 a
  // 3 span
  // 4 text
  let style = Arc::new(ComputedStyle::default());

  let mut dummy_text = BoxNode::new_text(style.clone(), "txt".to_string());
  dummy_text.styled_node_id = Some(4);

  let anonymous = BoxNode::new_anonymous_inline(style.clone(), vec![]);

  let mut span = BoxNode::new_inline(style.clone(), vec![dummy_text, anonymous]);
  span.styled_node_id = Some(3);

  let mut a_box = BoxNode::new_inline(style.clone(), vec![span]);
  a_box.styled_node_id = Some(2);

  let box_tree = BoxTree::new(a_box);
  let anonymous_box_id = box_tree.root.children[0].children[1].id;

  let hit_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
    anonymous_box_id,
    vec![],
  );
  let root_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    box_tree.root.id,
    vec![hit_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
    .expect("expected a hit result");
  assert_eq!(result.box_id, anonymous_box_id);
  assert_eq!(result.styled_node_id, 3);
  assert_eq!(result.dom_node_id, 2);
  assert_eq!(result.kind, HitTestKind::Link);
  assert_eq!(result.href.as_deref(), Some("/foo"));
}

#[test]
fn hit_test_dom_resolves_form_control() {
  let dom = doc(vec![elem(
    "input",
    vec![("id", "x"), ("type", "text")],
    vec![],
  )]);

  // DOM ids (pre-order):
  // 1 document
  // 2 input
  let style = Arc::new(ComputedStyle::default());
  let mut input_box = BoxNode::new_inline(style, vec![]);
  input_box.styled_node_id = Some(2);

  let box_tree = BoxTree::new(input_box);
  let input_box_id = box_tree.root.id;

  let root_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    input_box_id,
    vec![],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
    .expect("expected a hit result");
  assert_eq!(result.box_id, input_box_id);
  assert_eq!(result.styled_node_id, 2);
  assert_eq!(result.dom_node_id, 2);
  assert_eq!(result.kind, HitTestKind::FormControl);
  assert_eq!(result.href, None);
}

#[test]
fn hit_test_dom_skips_pointer_events_none() {
  let dom = doc(vec![elem(
    "div",
    vec![("id", "root")],
    vec![
      elem("a", vec![("href", "/ok")], vec![]),
      elem("div", vec![("id", "overlay")], vec![]),
    ],
  )]);

  // DOM ids (pre-order):
  // 1 document
  // 2 div#root
  // 3 a[href]
  // 4 div#overlay
  let style = Arc::new(ComputedStyle::default());
  let mut overlay_style = ComputedStyle::default();
  overlay_style.pointer_events = PointerEvents::None;
  let overlay_style = Arc::new(overlay_style);

  let mut link_box = BoxNode::new_inline(style.clone(), vec![]);
  link_box.styled_node_id = Some(3);

  let mut overlay_box = BoxNode::new_block(
    overlay_style,
    fastrender::FormattingContextType::Block,
    vec![],
  );
  overlay_box.styled_node_id = Some(4);

  let mut root_box = BoxNode::new_block(
    style,
    fastrender::FormattingContextType::Block,
    vec![link_box, overlay_box],
  );
  root_box.styled_node_id = Some(2);

  let box_tree = BoxTree::new(root_box);
  let link_box_id = box_tree.root.children[0].id;
  let overlay_box_id = box_tree.root.children[1].id;

  let link_fragment =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), link_box_id, vec![]);
  let overlay_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    overlay_box_id,
    vec![],
  );
  let root_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    box_tree.root.id,
    vec![link_fragment, overlay_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
    .expect("expected a hit result");

  assert_eq!(result.box_id, link_box_id);
  assert_eq!(result.dom_node_id, 3);
  assert_eq!(result.kind, HitTestKind::Link);
  assert_eq!(result.href.as_deref(), Some("/ok"));
}

#[test]
fn hit_test_dom_returns_none_for_inert_subtree() {
  let dom = doc(vec![elem(
    "a",
    vec![("href", "/foo"), ("inert", "")],
    vec![elem("span", vec![], vec![text("txt")])],
  )]);

  let style = Arc::new(ComputedStyle::default());

  let anonymous = BoxNode::new_anonymous_inline(style.clone(), vec![]);

  let mut span = BoxNode::new_inline(style.clone(), vec![anonymous]);
  span.styled_node_id = Some(3);

  let mut a_box = BoxNode::new_inline(style, vec![span]);
  a_box.styled_node_id = Some(2);

  let box_tree = BoxTree::new(a_box);
  let anonymous_box_id = box_tree.root.children[0].children[0].id;

  let hit_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
    anonymous_box_id,
    vec![],
  );
  let root_fragment = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    box_tree.root.id,
    vec![hit_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  assert_eq!(
    hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0)),
    None
  );
}

#[test]
fn resolve_label_associated_control_for_attribute() {
  let dom = doc(vec![
    elem("label", vec![("for", "x")], vec![text("Name")]),
    elem("input", vec![("id", "x"), ("type", "text")], vec![]),
  ]);

  // DOM ids:
  // 1 document
  // 2 label
  // 3 text
  // 4 input#x
  assert_eq!(resolve_label_associated_control(&dom, 2), Some(4));
}

#[test]
fn resolve_label_associated_control_descendant_input() {
  let dom = doc(vec![elem(
    "label",
    vec![],
    vec![elem("input", vec![("type", "text")], vec![]), text("Name")],
  )]);

  // DOM ids:
  // 1 document
  // 2 label
  // 3 input
  assert_eq!(resolve_label_associated_control(&dom, 2), Some(3));
}
