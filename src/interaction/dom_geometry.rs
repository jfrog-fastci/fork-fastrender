use crate::geometry::{Point, Rect, Size};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree, HitTestRoot};
use std::collections::HashMap;

/// Collect all non-generated box ids that originate from `styled_node_id`.
///
/// This is used as the shared mapping layer between DOM nodes (identified by their renderer
/// preorder id) and layout/painters (which use box ids).
pub fn collect_box_ids_for_styled_node(box_tree: &BoxTree, styled_node_id: usize) -> Vec<usize> {
  let mut out: Vec<usize> = Vec::new();
  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
      out.push(node.id);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

pub fn union_absolute_bounds_for_box_ids(fragment_tree: &FragmentTree, box_ids: &[usize]) -> Option<Rect> {
  let mut bounds: Option<Rect> = None;
  for &id in box_ids {
    let Some(rect) = crate::interaction::absolute_bounds_for_box_id(fragment_tree, id) else {
      continue;
    };
    bounds = Some(match bounds {
      Some(existing) => existing.union(rect),
      None => rect,
    });
  }
  bounds
}

pub fn union_scrolled_absolute_bounds_for_box_ids(
  fragment_tree: &FragmentTree,
  scroll: &ScrollState,
  box_ids: &[usize],
) -> Option<Rect> {
  let mut tree = fragment_tree.clone();
  crate::scroll::apply_scroll_offsets(&mut tree, scroll);
  crate::scroll::apply_viewport_scroll_cancel(&mut tree, scroll);
  union_absolute_bounds_for_box_ids(&tree, box_ids)
}

/// Compute viewport-local bounds for many DOM/styled node preorder ids.
///
/// The returned rectangles are in **viewport CSS pixel coordinates** (i.e. the coordinate space
/// used by pointer events and accessibility APIs).
///
/// - Element scroll offsets and sticky offsets are applied by cloning the prepared document's
///   fragment tree via [`crate::api::PreparedDocument::fragment_tree_for_geometry`].
/// - Viewport scroll (`scroll.viewport`) is then subtracted to convert page → viewport coordinates.
///
/// Note: [`crate::api::PreparedDocument::fragment_tree_for_geometry`] already applies paint-time
/// viewport-scroll cancel semantics for viewport-fixed (`position: fixed`) fragments by translating
/// them into page coordinates. Subtracting `scroll.viewport` therefore keeps those fixed elements
/// pinned to the viewport while normal content scrolls.
///
/// Nodes that do not produce any non-generated boxes (e.g. `display: none`, `display: contents`,
/// some text nodes) are **omitted** from the output map.
pub fn viewport_bounds_for_dom_node_ids(
  prepared: &crate::api::PreparedDocument,
  scroll: &ScrollState,
  node_ids: &[usize],
) -> HashMap<usize, Rect> {
  use rustc_hash::FxHashSet;

  if node_ids.is_empty() {
    return HashMap::new();
  }

  // 1) Map styled node ids -> box ids (non-generated only) by walking the box tree once.
  let requested: FxHashSet<usize> = node_ids.iter().copied().collect();
  let mut box_ids_by_node: HashMap<usize, Vec<usize>> = HashMap::new();

  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&prepared.box_tree().root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() {
      if let Some(styled_node_id) = node.styled_node_id {
        if requested.contains(&styled_node_id) {
          box_ids_by_node
            .entry(styled_node_id)
            .or_default()
            .push(node.id);
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  if box_ids_by_node.is_empty() {
    return HashMap::new();
  }

  // 2) Clone and translate the fragment tree into paint-time geometry coordinates (element scroll +
  // sticky), then collect bounds for every referenced box id in a single fragment-tree traversal.
  let fragment_tree = prepared.fragment_tree_for_geometry(scroll);

  let mut needed_box_ids: FxHashSet<usize> = FxHashSet::default();
  for ids in box_ids_by_node.values() {
    needed_box_ids.extend(ids.iter().copied());
  }

  let mut box_bounds: HashMap<usize, Rect> = HashMap::with_capacity(needed_box_ids.len());

  struct Frame<'a> {
    node: &'a FragmentNode,
    parent_offset: Point,
  }

  let mut frag_stack: Vec<Frame<'_>> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    frag_stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  frag_stack.push(Frame {
    node: &fragment_tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = frag_stack.pop() {
    let absolute_bounds = frame.node.bounds.translate(frame.parent_offset);
    if let Some(box_id) = frame.node.box_id() {
      if needed_box_ids.contains(&box_id) {
        box_bounds
          .entry(box_id)
          .and_modify(|existing| *existing = existing.union(absolute_bounds))
          .or_insert(absolute_bounds);
      }
    }

    let child_parent_offset = absolute_bounds.origin;
    for child in frame.node.children.iter().rev() {
      frag_stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  // 3) Union per-node bounds, then convert page → viewport coordinates.
  let mut out: HashMap<usize, Rect> = HashMap::new();
  let viewport_offset = Point::new(-scroll.viewport.x, -scroll.viewport.y);

  for &node_id in node_ids {
    let Some(box_ids) = box_ids_by_node.get(&node_id) else {
      continue;
    };

    let mut bounds: Option<Rect> = None;
    for &box_id in box_ids {
      let Some(rect) = box_bounds.get(&box_id).copied() else {
        continue;
      };
      bounds = Some(match bounds {
        Some(existing) => existing.union(rect),
        None => rect,
      });
    }
    let Some(mut rect) = bounds else {
      continue;
    };
    rect = rect.translate(viewport_offset);

    out.insert(node_id, rect);
  }

  out
}

fn find_fragment_path_within_root(root: &FragmentNode, box_id: usize) -> Option<Vec<usize>> {
  struct Frame<'a> {
    node: &'a FragmentNode,
    next_child: usize,
  }

  let mut path: Vec<usize> = Vec::new();
  let mut stack: Vec<Frame<'_>> = vec![Frame {
    node: root,
    next_child: 0,
  }];

  while let Some(frame) = stack.last_mut() {
    if frame.next_child == 0 && frame.node.box_id() == Some(box_id) {
      return Some(path.clone());
    }

    if frame.next_child < frame.node.children.len() {
      let idx = frame.next_child;
      frame.next_child += 1;
      let child = &frame.node.children[idx];
      path.push(idx);
      stack.push(Frame {
        node: child,
        next_child: 0,
      });
    } else {
      stack.pop();
      path.pop();
    }
  }

  None
}

pub fn find_first_fragment_path_for_box_id(
  fragment_tree: &FragmentTree,
  box_id: usize,
) -> Option<(HitTestRoot, Vec<usize>)> {
  if let Some(path) = find_fragment_path_within_root(&fragment_tree.root, box_id) {
    return Some((HitTestRoot::Root, path));
  }
  for (idx, root) in fragment_tree.additional_fragments.iter().enumerate() {
    if let Some(path) = find_fragment_path_within_root(root, box_id) {
      return Some((HitTestRoot::Additional(idx), path));
    }
  }
  None
}

pub fn resolve_fragment_path<'a>(
  fragment_tree: &'a FragmentTree,
  root_kind: HitTestRoot,
  path: &[usize],
) -> Option<(&'a FragmentNode, Point, bool)> {
  let mut node = match root_kind {
    HitTestRoot::Root => &fragment_tree.root,
    HitTestRoot::Additional(idx) => fragment_tree.additional_fragments.get(idx)?,
  };

  let mut origin = Point::new(node.bounds.x(), node.bounds.y());
  let mut has_fixed_cb_ancestor = false;

  for &idx in path {
    has_fixed_cb_ancestor = has_fixed_cb_ancestor
      || node
        .style
        .as_deref()
        .is_some_and(|style| style.establishes_fixed_containing_block());
    let child = node.children.get(idx)?;
    origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
    node = child;
  }

  Some((node, origin, has_fixed_cb_ancestor))
}

fn sanitize_nonneg(value: f32) -> f32 {
  if value.is_finite() { value.max(0.0) } else { 0.0 }
}

pub fn client_size_for_fragment(fragment: &FragmentNode) -> Size {
  let border_box_width = sanitize_nonneg(fragment.bounds.width());
  let border_box_height = sanitize_nonneg(fragment.bounds.height());

  let (border_left, border_right, border_top, border_bottom) = fragment
    .style
    .as_deref()
    .map(|style| {
      (
        sanitize_nonneg(style.used_border_left_width().to_px()),
        sanitize_nonneg(style.used_border_right_width().to_px()),
        sanitize_nonneg(style.used_border_top_width().to_px()),
        sanitize_nonneg(style.used_border_bottom_width().to_px()),
      )
    })
    .unwrap_or((0.0, 0.0, 0.0, 0.0));

  let reservation = fragment.scrollbar_reservation;
  let reserve_left = sanitize_nonneg(reservation.left);
  let reserve_right = sanitize_nonneg(reservation.right);
  let reserve_top = sanitize_nonneg(reservation.top);
  let reserve_bottom = sanitize_nonneg(reservation.bottom);

  let width = (border_box_width - border_left - border_right - reserve_left - reserve_right).max(0.0);
  let height = (border_box_height - border_top - border_bottom - reserve_top - reserve_bottom).max(0.0);
  Size::new(width, height)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Transform;
  use crate::style::types::BorderStyle;
  use crate::style::values::Length;
  use crate::style::ComputedStyle;
  use crate::style::display::FormattingContextType;
  use crate::tree::box_tree::{BoxNode, BoxTree, GeneratedPseudoElement};
  use crate::tree::fragment_tree::ScrollbarReservation;
  use std::sync::Arc;

  #[test]
  fn collect_box_ids_ignores_generated_pseudo_boxes() {
    let style = Arc::new(ComputedStyle::default());
    let mut principal = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    principal.styled_node_id = Some(10);

    let mut pseudo = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![])
      .with_generated_pseudo(GeneratedPseudoElement::Before);
    pseudo.styled_node_id = Some(10);

    let mut marker = BoxNode::new_marker(style.clone(), crate::tree::box_tree::MarkerContent::Text("•".into()));
    marker.styled_node_id = Some(10);

    let root = BoxNode::new_block(style, FormattingContextType::Block, vec![principal, pseudo, marker]);
    let tree = BoxTree::new(root);

    let ids = collect_box_ids_for_styled_node(&tree, 10);
    assert_eq!(ids.len(), 2, "expected principal + marker boxes, got {ids:?}");
  }

  #[test]
  fn find_first_fragment_path_prefers_earliest_preorder_match() {
    let root = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      1,
      vec![
        FragmentNode::new_block_with_id(Rect::from_xywh(10.0, 10.0, 10.0, 10.0), 2, vec![]),
        FragmentNode::new_block_with_id(
          Rect::from_xywh(20.0, 20.0, 10.0, 10.0),
          3,
          vec![FragmentNode::new_block_with_id(
            Rect::from_xywh(5.0, 5.0, 1.0, 1.0),
            2,
            vec![],
          )],
        ),
      ],
    );
    let tree = FragmentTree::new(root);

    let (root_kind, path) = find_first_fragment_path_for_box_id(&tree, 2).expect("path for box 2");
    assert_eq!(root_kind, HitTestRoot::Root);
    assert_eq!(path, vec![0]);
  }

  #[test]
  fn resolve_fragment_path_computes_absolute_origin_and_fixed_cb_ancestor() {
    let mut style = ComputedStyle::default();
    style.transform = vec![Transform::Scale(1.0, 1.0)];
    let style = Arc::new(style);

    let mut root = FragmentNode::new_block_with_id(
      Rect::from_xywh(5.0, 6.0, 100.0, 100.0),
      1,
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(10.0, 20.0, 10.0, 10.0),
        2,
        vec![],
      )],
    );
    root.style = Some(style);

    let tree = FragmentTree::new(root);
    let (node, origin, has_fixed_cb_ancestor) =
      resolve_fragment_path(&tree, HitTestRoot::Root, &[0]).expect("resolved");
    assert_eq!(node.box_id(), Some(2));
    assert_eq!(origin, Point::new(15.0, 26.0));
    assert!(has_fixed_cb_ancestor, "expected ancestor with fixed containing block");
  }

  #[test]
  fn client_size_for_fragment_subtracts_border_and_scrollbar_reservation() {
    let mut style = ComputedStyle::default();
    style.border_left_style = BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_style = BorderStyle::Solid;
    style.border_right_width = Length::px(4.0);
    style.border_top_style = BorderStyle::Solid;
    style.border_top_width = Length::px(1.0);
    style.border_bottom_style = BorderStyle::Solid;
    style.border_bottom_width = Length::px(3.0);

    let style = Arc::new(style);
    let mut fragment = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![], style);
    fragment.scrollbar_reservation = ScrollbarReservation {
      left: 1.0,
      right: 2.0,
      top: 0.5,
      bottom: 1.5,
    };

    let size = client_size_for_fragment(&fragment);
    assert_eq!(size, Size::new(91.0, 44.0));
  }
}
