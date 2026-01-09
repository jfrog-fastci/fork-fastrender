use crate::dom::{enumerate_dom_ids, DomNode, DomNodeType};
use crate::geometry::{Point, Size};
use crate::scroll::build_scroll_chain;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use percent_encoding::percent_decode_str;
use rustc_hash::FxHashSet;

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

fn find_target_dom_id(
  dom: &DomNode,
  fragment: &str,
  id_map: &std::collections::HashMap<*const DomNode, usize>,
) -> Option<usize> {
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
        return id_map.get(&(node as *const DomNode)).copied();
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

/// Compute a suggested viewport scroll offset for a same-document fragment navigation target.
///
/// The returned point aligns the top edge of the target's first fragment to the top edge of the
/// viewport (y-only for now).
pub fn scroll_offset_for_fragment_target(
  dom: &DomNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  fragment: &str,
  viewport: Size,
) -> Option<Point> {
  let decoded_fragment = percent_decode_str(fragment.trim_start_matches('#')).decode_utf8_lossy();
  let id_map = enumerate_dom_ids(dom);
  let target_dom_id = find_target_dom_id(dom, decoded_fragment.as_ref(), &id_map)?;

  // BoxTree: find all boxes produced by the target styled node.
  let mut target_box_ids: FxHashSet<usize> = FxHashSet::default();
  let mut box_stack = vec![&box_tree.root];
  while let Some(node) = box_stack.pop() {
    if node.styled_node_id == Some(target_dom_id) {
      target_box_ids.insert(node.id);
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

  // FragmentTree: find all fragments for those box ids, compute minimum absolute y.
  let mut min_target_y: Option<f32> = None;
  let mut frag_stack: Vec<(&crate::tree::fragment_tree::FragmentNode, Point)> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    frag_stack.push((root, Point::ZERO));
  }
  frag_stack.push((&fragment_tree.root, Point::ZERO));

  while let Some((fragment, parent_origin)) = frag_stack.pop() {
    let abs_bounds = fragment.bounds.translate(parent_origin);
    if let Some(box_id) = fragment.box_id() {
      if target_box_ids.contains(&box_id) {
        let y = abs_bounds.y();
        min_target_y = Some(min_target_y.map_or(y, |min| min.min(y)));
      }
    }

    let self_origin = abs_bounds.origin;
    for child in fragment.children.iter().rev() {
      frag_stack.push((child, self_origin));
    }
  }

  let Some(target_y) = min_target_y else {
    return None;
  };

  // Align top-of-target to top-of-viewport.
  let mut scroll_y = target_y;

  // Clamp to the document scroll range.
  //
  // Note: `FragmentTree::content_size()` includes additional fragment roots (e.g. viewport-fixed
  // layers). For scrolling we want the scroll bounds of the root scroll container, which:
  // - ignores viewport-fixed descendants, and
  // - can have a negative min when content extends into negative coordinates.
  let bounds = build_scroll_chain(&fragment_tree.root, viewport, &[])
    .first()
    .map(|state| state.bounds);
  if !scroll_y.is_finite() {
    scroll_y = 0.0;
  }
  if let Some(bounds) = bounds {
    scroll_y = bounds.clamp(Point::new(0.0, scroll_y)).y;
  } else {
    scroll_y = scroll_y.max(0.0);
  }

  Some(Point::new(0.0, scroll_y))
}
