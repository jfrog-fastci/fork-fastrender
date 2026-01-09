use std::sync::Arc;

use fastrender::dom::{self, DomNode, DomNodeType};
use fastrender::{
  scroll_offset_for_fragment_target, BoxNode, BoxTree, ComputedStyle, FormattingContextType,
  FragmentContent, FragmentNode, FragmentTree, Rect, Size,
};
use selectors::context::QuirksMode;

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn document_with_child(child: DomNode) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: vec![child],
  }
}

fn element(tag: &str, attributes: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: String::new(),
      attributes: attributes
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    },
    children,
  }
}

#[test]
fn anchor_scrolls_to_id_target_y() {
  let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
  let target_ptr = &dom.children[0] as *const DomNode;
  let id_map = dom::enumerate_dom_ids(&dom);
  let target_id = id_map[&target_ptr];

  let mut target_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  target_box.styled_node_id = Some(target_id);
  let root_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![target_box],
  );
  let box_tree = BoxTree::new(root_box);
  let target_box_id = box_tree.root.children[0].id;

  let target_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 500.0, 10.0, 10.0),
    FragmentContent::Block {
      box_id: Some(target_box_id),
    },
    vec![],
  );
  let root_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 1000.0),
    FragmentContent::Block {
      box_id: Some(box_tree.root.id),
    },
    vec![target_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let viewport = Size::new(10.0, 100.0);
  let offset =
    scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
      .expect("should find #target");
  assert_eq!(offset.x, 0.0);
  assert_eq!(offset.y, 500.0);
}

#[test]
fn anchor_scroll_clamps_to_max_scroll_y() {
  let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
  let target_ptr = &dom.children[0] as *const DomNode;
  let id_map = dom::enumerate_dom_ids(&dom);
  let target_id = id_map[&target_ptr];

  let mut target_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  target_box.styled_node_id = Some(target_id);
  let root_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![target_box],
  );
  let box_tree = BoxTree::new(root_box);
  let target_box_id = box_tree.root.children[0].id;

  // Content height 510, viewport height 100 => max scroll y = 410.
  let target_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 500.0, 10.0, 10.0),
    FragmentContent::Block {
      box_id: Some(target_box_id),
    },
    vec![],
  );
  let root_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 510.0),
    FragmentContent::Block {
      box_id: Some(box_tree.root.id),
    },
    vec![target_fragment],
  );
  let mut fragment_tree = FragmentTree::new(root_fragment);
  // Additional fragment roots (e.g. viewport-fixed layers) should not affect the viewport scroll
  // range used for anchor scrolling.
  fragment_tree.additional_fragments.push(FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 10_000.0),
    FragmentContent::Block { box_id: None },
    vec![],
  ));

  let viewport = Size::new(10.0, 100.0);
  let offset =
    scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
      .expect("should find #target");
  assert_eq!(offset.y, 410.0);
}

#[test]
fn anchor_scroll_allows_negative_scroll_bounds() {
  let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
  let target_ptr = &dom.children[0] as *const DomNode;
  let id_map = dom::enumerate_dom_ids(&dom);
  let target_id = id_map[&target_ptr];

  let mut target_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  target_box.styled_node_id = Some(target_id);
  let root_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![target_box],
  );
  let box_tree = BoxTree::new(root_box);
  let target_box_id = box_tree.root.children[0].id;

  let target_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, -50.0, 10.0, 10.0),
    FragmentContent::Block {
      box_id: Some(target_box_id),
    },
    vec![],
  );
  let root_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 100.0),
    FragmentContent::Block {
      box_id: Some(box_tree.root.id),
    },
    vec![target_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let viewport = Size::new(10.0, 100.0);
  let offset =
    scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
      .expect("should find #target");
  assert_eq!(
    offset.y, -50.0,
    "anchor scrolling should clamp using true scroll bounds (which can be negative)"
  );
}

#[test]
fn anchor_scroll_supports_a_name_targets() {
  let dom = document_with_child(element("a", vec![("name", "target")], vec![]));
  let target_ptr = &dom.children[0] as *const DomNode;
  let id_map = dom::enumerate_dom_ids(&dom);
  let target_id = id_map[&target_ptr];

  let mut target_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  target_box.styled_node_id = Some(target_id);
  let root_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![target_box],
  );
  let box_tree = BoxTree::new(root_box);
  let target_box_id = box_tree.root.children[0].id;

  let target_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 500.0, 10.0, 10.0),
    FragmentContent::Block {
      box_id: Some(target_box_id),
    },
    vec![],
  );
  let root_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 1000.0),
    FragmentContent::Block {
      box_id: Some(box_tree.root.id),
    },
    vec![target_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let viewport = Size::new(10.0, 100.0);
  let offset =
    scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
      .expect("should find <a name=target>");
  assert_eq!(offset.y, 500.0);
}

#[test]
fn anchor_scroll_supports_area_name_targets_by_scrolling_to_img() {
  let dom = document_with_child(element(
    "div",
    vec![],
    vec![
      element("img", vec![("id", "img"), ("usemap", "#m")], vec![]),
      element(
        "map",
        vec![("id", "m")],
        vec![element("area", vec![("name", "target")], vec![])],
      ),
    ],
  ));

  let img_ptr = &dom.children[0].children[0] as *const DomNode;
  let id_map = dom::enumerate_dom_ids(&dom);
  let img_id = id_map[&img_ptr];

  let mut img_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  img_box.styled_node_id = Some(img_id);
  let root_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![img_box],
  );
  let box_tree = BoxTree::new(root_box);
  let img_box_id = box_tree.root.children[0].id;

  let img_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 500.0, 10.0, 10.0),
    FragmentContent::Block {
      box_id: Some(img_box_id),
    },
    vec![],
  );
  let root_fragment = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 10.0, 1000.0),
    FragmentContent::Block {
      box_id: Some(box_tree.root.id),
    },
    vec![img_fragment],
  );
  let fragment_tree = FragmentTree::new(root_fragment);

  let viewport = Size::new(10.0, 100.0);
  let offset =
    scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
      .expect("should find <area name=target>");
  assert_eq!(offset.y, 500.0);
}
