use std::collections::HashMap;

use crate::geometry::{Point, Rect};
use crate::style::types::OverflowAnchor;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree, HitTestRoot};
use super::ScrollState;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollAnchor {
  /// Stable identifier for the anchor node (currently the originating box id).
  pub box_id: usize,
  /// Anchor origin relative to the scroll container's coordinate space.
  pub origin: Point,
}

/// A high-priority scroll anchoring candidate provided by the embedding/UI layer.
///
/// This is used to implement CSS Scroll Anchoring Module Level 1 §2.2 "anchor priority candidates",
/// such as the element containing the current active match of the find-in-page algorithm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollAnchoringPriorityCandidate {
  /// Identify the candidate by a stable box id.
  ///
  /// When `point` is provided, it should be a page coordinate within (or near) the target fragment,
  /// and is used to disambiguate cases where the same box id produces multiple fragments.
  BoxId { box_id: usize, point: Option<Point> },
  /// Fallback: identify the candidate by a page coordinate point.
  ///
  /// The engine will hit test this point to derive a fragment/box id.
  Point(Point),
}

/// Captured scroll anchoring state used to adjust scroll offsets across relayouts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScrollAnchorSnapshot {
  /// Anchor used for viewport scrolling.
  pub viewport: Option<ScrollAnchor>,
  /// Anchors used for element scroll containers, keyed by scroll container box id.
  pub elements: HashMap<usize, ScrollAnchor>,
}

fn point_add(a: Point, b: Point) -> Point {
  Point::new(a.x + b.x, a.y + b.y)
}

fn point_sub(a: Point, b: Point) -> Point {
  Point::new(a.x - b.x, a.y - b.y)
}

fn sanitize_point(p: Point) -> Point {
  Point::new(if p.x.is_finite() { p.x } else { 0.0 }, if p.y.is_finite() { p.y } else { 0.0 })
}

fn fragment_excludes_scroll_anchoring(node: &FragmentNode) -> bool {
  node
    .style
    .as_deref()
    .is_some_and(|style| style.overflow_anchor == OverflowAnchor::None)
}

fn fragment_is_anchor_candidate(node: &FragmentNode) -> bool {
  node.box_id().is_some() && !fragment_excludes_scroll_anchoring(node)
}

fn find_fragment_with_box_id<'a>(root: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  // DFS pre-order traversal, maintaining natural child order for determinism.
  let mut stack: Vec<&'a FragmentNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.box_id() == Some(box_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_fragment_by_box_id<'a>(tree: &'a FragmentTree, box_id: usize) -> Option<&'a FragmentNode> {
  if let Some(found) = find_fragment_with_box_id(&tree.root, box_id) {
    return Some(found);
  }
  for root in &tree.additional_fragments {
    if let Some(found) = find_fragment_with_box_id(root, box_id) {
      return Some(found);
    }
  }
  None
}

fn select_anchor_in_subtree(
  root: &FragmentNode,
  scrollport: Rect,
  start_origin: Point,
  include_root: bool,
) -> Option<ScrollAnchor> {
  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a FragmentNode,
    origin: Point,
    next_child: usize,
    consider: bool,
    excluded: bool,
  }

  let mut stack = vec![Frame {
    node: root,
    origin: start_origin,
    next_child: 0,
    consider: include_root,
    excluded: fragment_excludes_scroll_anchoring(root),
  }];

  while let Some(frame) = stack.last_mut() {
    if frame.next_child == 0 {
      if frame.consider && !frame.excluded && fragment_is_anchor_candidate(frame.node) {
        let rect = Rect::from_xywh(
          frame.origin.x,
          frame.origin.y,
          frame.node.bounds.width(),
          frame.node.bounds.height(),
        );
        if rect.intersects(scrollport) {
          if let Some(box_id) = frame.node.box_id() {
            return Some(ScrollAnchor {
              box_id,
              origin: frame.origin,
            });
          }
        }
      }
    }

    if frame.excluded {
      // `overflow-anchor: none` excludes the entire subtree from participating.
      stack.pop();
      continue;
    }

    if frame.next_child < frame.node.children.len() {
      let idx = frame.next_child;
      frame.next_child += 1;
      let child = &frame.node.children[idx];
      let origin = Point::new(frame.origin.x + child.bounds.x(), frame.origin.y + child.bounds.y());
      stack.push(Frame {
        node: child,
        origin,
        next_child: 0,
        consider: true,
        excluded: fragment_excludes_scroll_anchoring(child),
      });
    } else {
      stack.pop();
    }
  }

  None
}

fn find_anchor_origin_in_subtree(
  root: &FragmentNode,
  anchor_box_id: usize,
  start_origin: Point,
  include_root: bool,
) -> Option<Point> {
  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a FragmentNode,
    origin: Point,
    next_child: usize,
    consider: bool,
    excluded: bool,
  }

  let mut stack = vec![Frame {
    node: root,
    origin: start_origin,
    next_child: 0,
    consider: include_root,
    excluded: fragment_excludes_scroll_anchoring(root),
  }];

  while let Some(frame) = stack.last_mut() {
    if frame.next_child == 0 && frame.consider {
      if frame.excluded {
        stack.pop();
        continue;
      }
      if frame.node.box_id() == Some(anchor_box_id) {
        // Only return when the node participates in scroll anchoring. This mirrors anchor selection
        // and makes `overflow-anchor:none` behave like an absent node for adjustment purposes.
        if fragment_is_anchor_candidate(frame.node) {
          return Some(frame.origin);
        }
        return None;
      }
    }

    if frame.excluded {
      stack.pop();
      continue;
    }

    if frame.next_child < frame.node.children.len() {
      let idx = frame.next_child;
      frame.next_child += 1;
      let child = &frame.node.children[idx];
      let origin = Point::new(frame.origin.x + child.bounds.x(), frame.origin.y + child.bounds.y());
      stack.push(Frame {
        node: child,
        origin,
        next_child: 0,
        consider: true,
        excluded: fragment_excludes_scroll_anchoring(child),
      });
    } else {
      stack.pop();
    }
  }

  None
}

fn viewport_scrollport(tree: &FragmentTree, viewport_scroll: Point) -> Rect {
  let viewport = tree.viewport_size();
  Rect::from_xywh(viewport_scroll.x, viewport_scroll.y, viewport.width, viewport.height)
}

fn element_scrollport(container: &FragmentNode, offset: Point) -> Rect {
  Rect::from_xywh(
    offset.x,
    offset.y,
    container.bounds.width(),
    container.bounds.height(),
  )
}

fn select_viewport_anchor(tree: &FragmentTree, scroll: Point) -> Option<ScrollAnchor> {
  let scrollport = viewport_scrollport(tree, scroll);
  // The viewport scroll container is the whole fragment tree. We start the traversal at the root
  // fragment's children so we don't treat the synthetic root wrapper as an anchor candidate.
  let root_origin = Point::new(tree.root.bounds.x(), tree.root.bounds.y());
  for child in &tree.root.children {
    let origin = Point::new(root_origin.x + child.bounds.x(), root_origin.y + child.bounds.y());
    if let Some(anchor) = select_anchor_in_subtree(child, scrollport, origin, true) {
      return Some(anchor);
    }
  }
  None
}

fn select_viewport_anchor_with_priority_candidate(
  tree: &FragmentTree,
  scroll: Point,
  priority: Option<ScrollAnchoringPriorityCandidate>,
) -> Option<ScrollAnchor> {
  if let Some(candidate) = priority {
    let scrollport = viewport_scrollport(tree, scroll);
    if let Some(anchor) = viewport_anchor_for_priority_candidate(tree, scrollport, candidate) {
      return Some(anchor);
    }
  }
  select_viewport_anchor(tree, scroll)
}

fn viewport_anchor_for_priority_candidate(
  tree: &FragmentTree,
  scrollport: Rect,
  candidate: ScrollAnchoringPriorityCandidate,
) -> Option<ScrollAnchor> {
  match candidate {
    ScrollAnchoringPriorityCandidate::BoxId { box_id, point } => {
      viewport_anchor_for_box_id(tree, scrollport, box_id, point)
    }
    ScrollAnchoringPriorityCandidate::Point(point) => viewport_anchor_for_point(tree, scrollport, point),
  }
}

fn score_origin_relative_to_point(origin: Point, rect: Rect, target: Point) -> f32 {
  if rect.contains_point(target) {
    0.0
  } else {
    let dx = (origin.x - target.x).abs();
    let dy = (origin.y - target.y).abs();
    // Prefer vertical closeness since vertical scrolling is the common case.
    dy * 1_000.0 + dx
  }
}

fn best_anchor_for_box_id_in_subtree(
  root: &FragmentNode,
  box_id: usize,
  scrollport: Rect,
  start_origin: Point,
  include_root: bool,
  target_point: Option<Point>,
) -> Option<(Point, f32)> {
  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a FragmentNode,
    origin: Point,
    next_child: usize,
    consider: bool,
    excluded: bool,
  }

  let mut best: Option<(Point, f32)> = None;
  let mut stack = vec![Frame {
    node: root,
    origin: start_origin,
    next_child: 0,
    consider: include_root,
    excluded: fragment_excludes_scroll_anchoring(root),
  }];

  while let Some(frame) = stack.last_mut() {
    if frame.next_child == 0
      && frame.consider
      && !frame.excluded
      && frame.node.box_id() == Some(box_id)
      && fragment_is_anchor_candidate(frame.node)
    {
      let rect = Rect::from_xywh(
        frame.origin.x,
        frame.origin.y,
        frame.node.bounds.width(),
        frame.node.bounds.height(),
      );
      if rect.intersects(scrollport) {
        let score = target_point.map_or(0.0, |p| score_origin_relative_to_point(frame.origin, rect, p));
        let replace = match best {
          None => true,
          Some((_, best_score)) => score < best_score,
        };
        if replace {
          best = Some((frame.origin, score));
          if score == 0.0 {
            // We found a fragment that actually contains the probe point; this is as good as it gets.
            // Continue scanning so we keep deterministic traversal order for ties.
          }
        }
      }
    }

    if frame.excluded {
      stack.pop();
      continue;
    }

    if frame.next_child < frame.node.children.len() {
      let idx = frame.next_child;
      frame.next_child += 1;
      let child = &frame.node.children[idx];
      let origin = Point::new(frame.origin.x + child.bounds.x(), frame.origin.y + child.bounds.y());
      stack.push(Frame {
        node: child,
        origin,
        next_child: 0,
        consider: true,
        excluded: fragment_excludes_scroll_anchoring(child),
      });
    } else {
      stack.pop();
    }
  }

  best
}

fn viewport_anchor_for_box_id(
  tree: &FragmentTree,
  scrollport: Rect,
  box_id: usize,
  target_point: Option<Point>,
) -> Option<ScrollAnchor> {
  // Walk the main fragment root only; additional fragment roots represent viewport-fixed layers and
  // do not participate in viewport scroll anchoring.
  let root_origin = Point::new(tree.root.bounds.x(), tree.root.bounds.y());
  let best = best_anchor_for_box_id_in_subtree(
    &tree.root,
    box_id,
    scrollport,
    root_origin,
    false,
    target_point,
  )?;
  Some(ScrollAnchor {
    box_id,
    origin: best.0,
  })
}

fn viewport_anchor_for_point(
  tree: &FragmentTree,
  scrollport: Rect,
  point: Point,
) -> Option<ScrollAnchor> {
  if !point.x.is_finite() || !point.y.is_finite() || !scrollport.contains_point(point) {
    return None;
  }

  let (root, path) = tree.hit_test_path(point)?;
  if !matches!(root, HitTestRoot::Root) {
    // Ignore additional fragment roots (e.g. fixed layers).
    return None;
  }

  // Walk down the hit-test path and pick the deepest eligible box id.
  let mut current = &tree.root;
  if fragment_excludes_scroll_anchoring(current) {
    return None;
  }
  let mut parent_origin = Point::ZERO;
  let mut current_bounds = current.bounds.translate(parent_origin);
  let mut best = current
    .box_id()
    .filter(|_| fragment_is_anchor_candidate(current))
    .map(|id| (id, current_bounds.origin));

  for &child_idx in &path {
    let Some(child) = current.children.get(child_idx) else {
      break;
    };
    parent_origin = current_bounds.origin;
    current = child;
    if fragment_excludes_scroll_anchoring(current) {
      return None;
    }
    current_bounds = current.bounds.translate(parent_origin);
    if let Some(id) = current.box_id() {
      if fragment_is_anchor_candidate(current) {
        best = Some((id, current_bounds.origin));
      }
    }
  }

  best.map(|(box_id, origin)| ScrollAnchor { box_id, origin })
}

fn select_element_anchor(
  tree: &FragmentTree,
  container_box_id: usize,
  scroll: Point,
) -> Option<ScrollAnchor> {
  let container = find_fragment_by_box_id(tree, container_box_id)?;
  let scrollport = element_scrollport(container, scroll);
  // Container local coordinates start at (0,0) for its children.
  for child in &container.children {
    let origin = Point::new(child.bounds.x(), child.bounds.y());
    if let Some(anchor) = select_anchor_in_subtree(child, scrollport, origin, true) {
      // Adjust origin to be relative to the container.
      return Some(ScrollAnchor {
        box_id: anchor.box_id,
        origin: anchor.origin,
      });
    }
  }
  None
}

/// Capture the current scroll anchor selections for the provided fragment tree + scroll state.
pub fn capture_scroll_anchors(tree: &FragmentTree, scroll: &ScrollState) -> ScrollAnchorSnapshot {
  capture_scroll_anchors_with_priority(tree, scroll, None)
}

/// Like [`capture_scroll_anchors`], but allows the embedding/UI layer to inject a higher-priority
/// viewport anchoring candidate.
pub fn capture_scroll_anchors_with_priority(
  tree: &FragmentTree,
  scroll: &ScrollState,
  priority: Option<ScrollAnchoringPriorityCandidate>,
) -> ScrollAnchorSnapshot {
  let mut snapshot = ScrollAnchorSnapshot::default();

  let viewport_scroll = sanitize_point(scroll.viewport);
  snapshot.viewport =
    select_viewport_anchor_with_priority_candidate(tree, viewport_scroll, priority);

  for (&container_id, &offset) in &scroll.elements {
    let offset = sanitize_point(offset);
    if let Some(anchor) = select_element_anchor(tree, container_id, offset) {
      snapshot.elements.insert(container_id, anchor);
    }
  }

  snapshot
}

fn apply_one_adjustment(
  current_scroll: Point,
  prev_anchor: ScrollAnchor,
  new_anchor_origin: Point,
) -> Point {
  let delta = point_sub(new_anchor_origin, prev_anchor.origin);
  point_add(current_scroll, delta)
}

/// Apply scroll anchoring adjustments to a scroll state given anchors captured from the previous
/// layout.
///
/// If a previously-selected anchor cannot be found (or no longer participates in scroll anchoring,
/// e.g. `overflow-anchor:none`), no adjustment is applied for that container. A fresh anchor is
/// still selected for the new layout so subsequent relayouts remain stable.
pub fn apply_scroll_anchoring(
  previous: &ScrollAnchorSnapshot,
  new_tree: &FragmentTree,
  scroll: &ScrollState,
) -> (ScrollState, ScrollAnchorSnapshot) {
  let mut next_scroll = scroll.clone();
  let mut next_snapshot = ScrollAnchorSnapshot::default();

  // Viewport scroll anchoring.
  if let Some(prev_anchor) = previous.viewport {
    let root_origin = Point::new(new_tree.root.bounds.x(), new_tree.root.bounds.y());
    let new_origin = find_anchor_origin_in_subtree(
      &new_tree.root,
      prev_anchor.box_id,
      root_origin,
      false,
    );
    if let Some(new_origin) = new_origin {
      next_scroll.viewport = apply_one_adjustment(sanitize_point(next_scroll.viewport), prev_anchor, new_origin);
      next_snapshot.viewport = Some(ScrollAnchor {
        box_id: prev_anchor.box_id,
        origin: new_origin,
      });
    } else {
      // Anchor missing/ineligible; no adjustment.
      next_scroll.viewport = sanitize_point(next_scroll.viewport);
      next_snapshot.viewport = select_viewport_anchor(new_tree, next_scroll.viewport);
    }
  } else {
    next_scroll.viewport = sanitize_point(next_scroll.viewport);
    next_snapshot.viewport = select_viewport_anchor(new_tree, next_scroll.viewport);
  }

  // Element scroll container anchoring.
  for (&container_id, &prev_anchor) in &previous.elements {
    let current_offset = sanitize_point(next_scroll.elements.get(&container_id).copied().unwrap_or(Point::ZERO));
    let Some(container) = find_fragment_by_box_id(new_tree, container_id) else {
      continue;
    };

    let new_origin =
      find_anchor_origin_in_subtree(container, prev_anchor.box_id, Point::ZERO, false);
    if let Some(new_origin) = new_origin {
      let adjusted = apply_one_adjustment(current_offset, prev_anchor, new_origin);
      next_scroll.elements.insert(container_id, adjusted);
      next_snapshot.elements.insert(
        container_id,
        ScrollAnchor {
          box_id: prev_anchor.box_id,
          origin: new_origin,
        },
      );
    } else {
      // Anchor missing/ineligible; no adjustment.
      next_scroll.elements.insert(container_id, current_offset);
      if let Some(anchor) = select_element_anchor(new_tree, container_id, current_offset) {
        next_snapshot.elements.insert(container_id, anchor);
      }
    }
  }

  // Ensure newly-scrolled containers also have anchors for future relayouts.
  for (&container_id, &offset) in &next_scroll.elements {
    if next_snapshot.elements.contains_key(&container_id) {
      continue;
    }
    if let Some(anchor) = select_element_anchor(new_tree, container_id, sanitize_point(offset)) {
      next_snapshot.elements.insert(container_id, anchor);
    }
  }

  // Keep a canonical representation matching other scroll routines: missing vs zero offsets should
  // not create spurious diffs.
  next_scroll
    .elements
    .retain(|_, offset| *offset != Point::ZERO);

  (next_scroll, next_snapshot)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Size;
  use crate::tree::fragment_tree::{FragmentContent, FragmentTree};
  use crate::ComputedStyle;
  use std::sync::Arc;

  #[test]
  fn overflow_anchor_none_excludes_subtree() {
    let mut excluded_style = ComputedStyle::default();
    excluded_style.overflow_anchor = OverflowAnchor::None;
    let excluded_style = Arc::new(excluded_style);

    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);
    let excluded = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Block { box_id: Some(1) },
      vec![FragmentNode::new_with_style(
        Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        FragmentContent::Block { box_id: Some(2) },
        vec![],
        Arc::new(ComputedStyle::default()),
      )],
      excluded_style,
    );
    root.children_mut().push(excluded);
    let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));

    let anchor = select_viewport_anchor(&tree, Point::ZERO);
    assert!(anchor.is_none(), "excluded subtree should produce no anchor");
  }
}
