use std::collections::HashMap;
use std::sync::Arc;

use fastrender::dom::{DomNode, DomNodeType};
use fastrender::interaction::{fragment_tree_with_scroll, hit_test_dom, HitTestKind};
use fastrender::scroll::ScrollState;
use fastrender::style::position::Position;
use fastrender::style::types::Overflow;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{
  BoxTree, DebugInfo, FormattingContextType, FragmentContent, FragmentNode, FragmentTree, Point,
  Rect,
};
use selectors::context::QuirksMode;

fn box_id_by_element_id(node: &BoxNode, target_id: &str) -> Option<usize> {
  if let Some(debug) = node.debug_info.as_ref() {
    if debug.id.as_deref() == Some(target_id) {
      return Some(node.id);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| box_id_by_element_id(child, target_id))
}

fn box_id_by_styled_node_id(node: &BoxNode, target_id: usize) -> Option<usize> {
  if node.styled_node_id == Some(target_id) {
    return Some(node.id);
  }
  node
    .children
    .iter()
    .find_map(|child| box_id_by_styled_node_id(child, target_id))
}

fn doc(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
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
fn hit_testing_fixed_inside_scroller_ignores_element_scroll_offsets() {
  // This test is intentionally data-structure level (no full `FastRender` pipeline) to keep the
  // interaction suite lightweight and avoid relying on font/image initialization. We only need to
  // validate that applying element scroll offsets doesn't move `position: fixed` fragments.
  //
  // DOM ids (pre-order):
  // 1 document
  // 2 div#scroller
  // 3 a#fixed
  // 4 text
  // 5 div#spacer
  let dom = doc(vec![elem(
    "div",
    vec![("id", "scroller")],
    vec![
      elem("a", vec![("id", "fixed"), ("href", "/ok")], vec![text("Target")]),
      elem("div", vec![("id", "spacer")], vec![]),
    ],
  )]);

  let base_style = Arc::new(ComputedStyle::default());

  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let mut fixed_style = ComputedStyle::default();
  fixed_style.position = Position::Fixed;
  let fixed_style = Arc::new(fixed_style);

  let mut fixed_box = BoxNode::new_block(fixed_style.clone(), FormattingContextType::Block, vec![]);
  fixed_box.debug_info = Some(DebugInfo::new(
    Some("a".to_string()),
    Some("fixed".to_string()),
    vec![],
  ));
  fixed_box.styled_node_id = Some(3);

  let mut spacer_box = BoxNode::new_block(base_style.clone(), FormattingContextType::Block, vec![]);
  spacer_box.debug_info = Some(DebugInfo::new(
    Some("div".to_string()),
    Some("spacer".to_string()),
    vec![],
  ));
  spacer_box.styled_node_id = Some(5);

  let mut scroller_box = BoxNode::new_block(
    scroller_style.clone(),
    FormattingContextType::Block,
    vec![fixed_box, spacer_box],
  );
  scroller_box.debug_info = Some(DebugInfo::new(
    Some("div".to_string()),
    Some("scroller".to_string()),
    vec![],
  ));
  scroller_box.styled_node_id = Some(2);

  let box_tree = BoxTree::new(scroller_box);

  let scroller_id = box_id_by_element_id(&box_tree.root, "scroller").expect("scroller box id");
  let fixed_id = box_id_by_styled_node_id(&box_tree.root, 3).expect("fixed box id");
  let spacer_id = box_id_by_styled_node_id(&box_tree.root, 5).expect("spacer box id");

  let fixed_fragment = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    FragmentContent::Block {
      box_id: Some(fixed_id),
    },
    vec![],
    fixed_style,
  );
  let spacer_fragment = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 20.0, 100.0, 200.0),
    FragmentContent::Block {
      box_id: Some(spacer_id),
    },
    vec![],
    base_style,
  );
  let root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
    FragmentContent::Block {
      box_id: Some(scroller_id),
    },
    // Paint order: later children are considered on top by `FragmentNode::fragments_at_point`.
    // In browsers, positioned elements (including `position: fixed`) paint above in-flow content,
    // so model that by ordering the fixed fragment after the scrollable content fragment.
    vec![spacer_fragment, fixed_fragment],
    scroller_style,
  );
  let fragment_tree = FragmentTree::new(root);

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(scroller_id, Point::new(0.0, 25.0))]),
  );

  let scrolled_tree = fragment_tree_with_scroll(&fragment_tree, &scroll_state);
  let result =
    hit_test_dom(&dom, &box_tree, &scrolled_tree, Point::new(5.0, 5.0)).expect("hit result");
  assert_eq!(result.kind, HitTestKind::Link);
  assert_eq!(result.href.as_deref(), Some("/ok"));
}
