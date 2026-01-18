use crate::dom::{enumerate_dom_ids, DomNode, DomNodeType};
use crate::geometry::{Point, Size};
use crate::scroll::viewport_scroll_bounds;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use crate::percent::percent_decode_str;
use rustc_hash::FxHashSet;

use super::image_maps;

fn node_is_inert_like(node: &DomNode) -> bool {
  matches!(
    node.node_type,
    DomNodeType::Element { .. } | DomNodeType::Slot { .. }
  ) && (node.get_attribute_ref("inert").is_some()
    || node
      .get_attribute_ref("data-fastr-inert")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false))
}

fn node_matches_id(node: &DomNode, fragment: &str) -> bool {
  node
    .get_attribute_ref("id")
    .is_some_and(|id| id == fragment)
}

fn node_matches_name_anchor(node: &DomNode, fragment: &str) -> bool {
  let Some(tag) = node.tag_name() else {
    return false;
  };
  if !(tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area")) {
    return false;
  }
  node
    .get_attribute_ref("name")
    .is_some_and(|name| name == fragment)
}

fn find_target_dom_node<'a>(dom: &'a DomNode, fragment: &str) -> Option<&'a DomNode> {
  if fragment.is_empty() {
    return None;
  }

  // Per HTML fragment semantics (and `:target`), an `id` match takes precedence over
  // `<a name=...>` / `<area name=...>`.
  for pass in 0..2 {
    let mut stack: Vec<&DomNode> = vec![dom];
    while let Some(node) = stack.pop() {
      let matches = if pass == 0 {
        node_matches_id(node, fragment)
      } else {
        node_matches_name_anchor(node, fragment)
      };
      if matches {
        return Some(node);
      }

      // Ignore nodes inside `<template>` contents, inert subtrees, and shadow roots.
      if node.is_template_element()
        || node_is_inert_like(node)
        || matches!(node.node_type, DomNodeType::ShadowRoot { .. })
      {
        continue;
      }

      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  None
}

fn nearest_map_ancestor<'a>(dom: &'a DomNode, target: *const DomNode) -> Option<&'a DomNode> {
  let mut stack: Vec<(&DomNode, Option<&DomNode>)> = vec![(dom, None)];
  while let Some((node, mut nearest)) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("map"))
    {
      nearest = Some(node);
    }
    if std::ptr::eq(node as *const DomNode, target) {
      return nearest;
    }

    // Keep traversal semantics consistent with `find_target_dom_node`.
    if node.is_template_element()
      || node_is_inert_like(node)
      || matches!(node.node_type, DomNodeType::ShadowRoot { .. })
    {
      continue;
    }

    for child in node.children.iter().rev() {
      stack.push((child, nearest));
    }
  }
  None
}

/// Compute a suggested viewport scroll offset for a same-document fragment navigation target.
///
/// The returned point aligns the start edges of the target's first fragment to the start edges of
/// the viewport (x and y).
pub fn scroll_offset_for_fragment_target(
  dom: &DomNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  fragment: &str,
  viewport: Size,
) -> Option<Point> {
  let decoded_fragment = percent_decode_str(fragment.trim_start_matches('#')).decode_utf8_lossy();
  let id_map = enumerate_dom_ids(dom);
  let target_node = find_target_dom_node(dom, decoded_fragment.as_ref())?;

  let mut target_dom_id = *id_map.get(&(target_node as *const DomNode))?;

  // `<area>` fragment navigations target a location on the associated `<img usemap>`, since `<area>`
  // itself has no layout boxes.
  if target_node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("area"))
  {
    let map = nearest_map_ancestor(dom, target_node as *const DomNode)?;
    let img = image_maps::first_img_referencing_map(dom, map as *const DomNode)?;
    target_dom_id = *id_map.get(&(img as *const DomNode))?;
  }

  // BoxTree: find all boxes produced by the target styled node.
  let mut target_box_ids: FxHashSet<usize> = FxHashSet::default();
  let mut scroll_margin_top: Option<f32> = None;
  let mut scroll_margin_left: Option<f32> = None;
  let mut box_stack = vec![&box_tree.root];
  while let Some(node) = box_stack.pop() {
    if node.styled_node_id == Some(target_dom_id) {
      target_box_ids.insert(node.id);
      if scroll_margin_top.is_none() || scroll_margin_left.is_none() {
        if scroll_margin_top.is_none() {
          let top = node.style.scroll_margin_top.to_px();
          scroll_margin_top = Some(if top.is_finite() { top } else { 0.0 });
        }
        if scroll_margin_left.is_none() {
          let left = node.style.scroll_margin_left.to_px();
          scroll_margin_left = Some(if left.is_finite() { left } else { 0.0 });
        }
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      box_stack.push(body);
    }
    for child in node.children.iter().rev() {
      box_stack.push(child);
    }
  }
  if target_box_ids.is_empty() {
    return None;
  }

  // FragmentTree: find all fragments for those box ids, compute minimum absolute x/y.
  let mut min_target_y: Option<f32> = None;
  let mut min_target_x: Option<f32> = None;
  let mut frag_stack: Vec<(&crate::tree::fragment_tree::FragmentNode, Point)> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    frag_stack.push((root, Point::ZERO));
  }
  frag_stack.push((&fragment_tree.root, Point::ZERO));

  while let Some((fragment, parent_origin)) = frag_stack.pop() {
    let abs_bounds = fragment.bounds.translate(parent_origin);
    if let Some(box_id) = fragment.box_id() {
      if target_box_ids.contains(&box_id) {
        let x = abs_bounds.x();
        let y = abs_bounds.y();
        min_target_x = Some(min_target_x.map_or(x, |min| min.min(x)));
        min_target_y = Some(min_target_y.map_or(y, |min| min.min(y)));
      }
    }

    let self_origin = abs_bounds.origin;
    for child in fragment.children.iter().rev() {
      frag_stack.push((child, self_origin));
    }
  }

  let (Some(target_x), Some(target_y)) = (min_target_x, min_target_y) else {
    return None;
  };

  // Align start-of-target to start-of-viewport.
  let mut scroll_x = target_x;
  let mut scroll_y = target_y;

  // Best-effort support for `scroll-margin-*`, matching the UA behavior for `scrollIntoView()`.
  if let Some(margin_left) = scroll_margin_left {
    scroll_x -= margin_left;
  }
  if let Some(margin_top) = scroll_margin_top {
    scroll_y -= margin_top;
  }

  // Clamp to the document scroll range.
  //
  // Note: `FragmentTree::content_size()` includes additional fragment roots (e.g. viewport-fixed
  // layers). For scrolling we want the scroll bounds of the root scroll container, which:
  // - ignores viewport-fixed descendants, and
  // - matches browser scroll range semantics (negative overflow does not allow negative scroll
  //   offsets).
  let bounds = viewport_scroll_bounds(&fragment_tree.root, viewport);
  if !scroll_x.is_finite() {
    scroll_x = 0.0;
  }
  if !scroll_y.is_finite() {
    scroll_y = 0.0;
  }
  let clamped = bounds.clamp(Point::new(scroll_x, scroll_y));
  scroll_x = clamped.x;
  scroll_y = clamped.y;

  Some(Point::new(scroll_x, scroll_y))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use crate::dom::{self, DomNode, DomNodeType};
  use crate::{
    BoxNode, BoxTree, ComputedStyle, FormattingContextType, FragmentContent, FragmentNode,
    FragmentTree, Length, Rect, Size,
  };
  use selectors::context::QuirksMode;

  use super::scroll_offset_for_fragment_target;

  fn default_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  fn document_with_child(child: DomNode) -> DomNode {
    DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
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

    let mut target_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
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

    let mut target_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
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
  fn anchor_scroll_clamps_negative_targets_to_zero() {
    let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
    let target_ptr = &dom.children[0] as *const DomNode;
    let id_map = dom::enumerate_dom_ids(&dom);
    let target_id = id_map[&target_ptr];

    let mut target_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
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
      offset.y, 0.0,
      "anchor scrolling should clamp to the minimum scroll offset (0)"
    );
  }

  #[test]
  fn anchor_scroll_supports_a_name_targets() {
    let dom = document_with_child(element("a", vec![("name", "target")], vec![]));
    let target_ptr = &dom.children[0] as *const DomNode;
    let id_map = dom::enumerate_dom_ids(&dom);
    let target_id = id_map[&target_ptr];

    let mut target_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
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
    let root_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![img_box]);
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

  #[test]
  fn anchor_scrolls_to_id_target_x() {
    let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
    let target_ptr = &dom.children[0] as *const DomNode;
    let id_map = dom::enumerate_dom_ids(&dom);
    let target_id = id_map[&target_ptr];

    let mut target_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    target_box.styled_node_id = Some(target_id);
    let root_box = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![target_box],
    );
    let box_tree = BoxTree::new(root_box);
    let target_box_id = box_tree.root.children[0].id;

    let target_fragment = FragmentNode::new(
      Rect::from_xywh(500.0, 0.0, 10.0, 10.0),
      FragmentContent::Block {
        box_id: Some(target_box_id),
      },
      vec![],
    );
    // Content width 1000, viewport width 100 => max scroll x = 900.
    let root_fragment = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 1000.0, 100.0),
      FragmentContent::Block {
        box_id: Some(box_tree.root.id),
      },
      vec![target_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let viewport = Size::new(100.0, 100.0);
    let offset =
      scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
        .expect("should find #target");
    assert_eq!(offset.x, 500.0);
    assert_eq!(offset.y, 0.0);
  }

  #[test]
  fn anchor_scroll_applies_scroll_margin() {
    let dom = document_with_child(element("div", vec![("id", "target")], vec![]));
    let target_ptr = &dom.children[0] as *const DomNode;
    let id_map = dom::enumerate_dom_ids(&dom);
    let target_id = id_map[&target_ptr];

    let mut style = ComputedStyle::default();
    style.scroll_margin_top = Length::px(10.0);
    style.scroll_margin_left = Length::px(20.0);
    let target_style = Arc::new(style);

    let mut target_box = BoxNode::new_block(target_style, FormattingContextType::Block, vec![]);
    target_box.styled_node_id = Some(target_id);
    let root_box = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![target_box],
    );
    let box_tree = BoxTree::new(root_box);
    let target_box_id = box_tree.root.children[0].id;

    let target_fragment = FragmentNode::new(
      Rect::from_xywh(100.0, 100.0, 10.0, 10.0),
      FragmentContent::Block {
        box_id: Some(target_box_id),
      },
      vec![],
    );
    let root_fragment = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 1000.0, 1000.0),
      FragmentContent::Block {
        box_id: Some(box_tree.root.id),
      },
      vec![target_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let viewport = Size::new(100.0, 100.0);
    let offset =
      scroll_offset_for_fragment_target(&dom, &box_tree, &fragment_tree, "target", viewport)
        .expect("should find #target");
    assert_eq!(offset.x, 80.0);
    assert_eq!(offset.y, 90.0);
  }
}
