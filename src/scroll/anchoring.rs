use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::geometry::{Point, Rect, Size};
use crate::style::display::Display;
use crate::style::position::Position;
use crate::style::types::{Direction, Overflow, OverflowAnchor, WritingMode};
use crate::style::ComputedStyle;
use crate::style::{block_axis_positive, inline_axis_is_horizontal, inline_axis_positive};
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

fn sanitize_point(p: Point) -> Point {
  Point::new(
    if p.x.is_finite() { p.x } else { 0.0 },
    if p.y.is_finite() { p.y } else { 0.0 },
  )
}

fn writing_mode_and_direction_from_style(
  style: Option<&crate::ComputedStyle>,
) -> (WritingMode, Direction) {
  style
    .map(|style| (style.writing_mode, style.direction))
    .unwrap_or((WritingMode::HorizontalTb, Direction::Ltr))
}

fn axis_signs_for_scroll_state(writing_mode: WritingMode, direction: Direction) -> (f32, f32) {
  // Scroll offsets are expressed as logical-start-relative distances (non-negative). Convert a
  // physical delta (fragment movement in physical x/y coordinates) into a scroll offset delta by
  // applying the axis sign: for axes where increasing scroll moves the viewport towards negative
  // coordinates, the delta must be negated.
  let x_is_inline = inline_axis_is_horizontal(writing_mode);
  let x_positive = if x_is_inline {
    inline_axis_positive(writing_mode, direction)
  } else {
    block_axis_positive(writing_mode)
  };
  let y_is_inline = !x_is_inline;
  let y_positive = if y_is_inline {
    inline_axis_positive(writing_mode, direction)
  } else {
    block_axis_positive(writing_mode)
  };

  (
    if x_positive { 1.0 } else { -1.0 },
    if y_positive { 1.0 } else { -1.0 },
  )
}

fn point_is_finite(p: Point) -> bool {
  p.x.is_finite() && p.y.is_finite()
}

fn rect_is_finite(rect: Rect) -> bool {
  point_is_finite(rect.origin) && rect.size.width.is_finite() && rect.size.height.is_finite()
}

fn checked_point_add(a: Point, b: Point) -> Option<Point> {
  let x = a.x + b.x;
  let y = a.y + b.y;
  (x.is_finite() && y.is_finite()).then_some(Point::new(x, y))
}

fn checked_point_sub(a: Point, b: Point) -> Option<Point> {
  let x = a.x - b.x;
  let y = a.y - b.y;
  (x.is_finite() && y.is_finite()).then_some(Point::new(x, y))
}

fn point_add(a: Point, b: Point) -> Point {
  Point::new(a.x + b.x, a.y + b.y)
}

fn point_sub(a: Point, b: Point) -> Point {
  Point::new(a.x - b.x, a.y - b.y)
}

fn checked_translate(origin: Point, delta: Point) -> Option<Point> {
  checked_point_add(origin, delta)
}

fn checked_rect_for_node(origin: Point, node: &FragmentNode) -> Option<Rect> {
  let width = node.bounds.width();
  let height = node.bounds.height();
  if point_is_finite(origin) && width.is_finite() && height.is_finite() {
    Some(Rect::from_xywh(origin.x, origin.y, width, height))
  } else {
    None
  }
}

fn approx_eq_point(a: Point, b: Point, epsilon: f32) -> bool {
  (a.x - b.x).abs() <= epsilon && (a.y - b.y).abs() <= epsilon
}

fn fragment_excludes_scroll_anchoring(node: &FragmentNode) -> bool {
  node
    .style
    .as_deref()
    .is_some_and(|style| {
      style.overflow_anchor == OverflowAnchor::None
        || style.display.is_none()
        || matches!(style.position, Position::Fixed)
    })
}

fn fragment_is_non_atomic_inline(node: &FragmentNode) -> bool {
  // CSS Scroll Anchoring §2.2: "An anchor node can be any box except one for a non-atomic inline."
  //
  // In FastRender, inline box fragments report `content.is_inline()`. Atomic inline-level boxes
  // (e.g. `display:inline-block`) may still produce an inline fragment; use `display:inline` to
  // identify non-atomic inlines.
  node.content.is_inline()
    && node
      .style
      .as_deref()
      .is_none_or(|style| style.display == Display::Inline)
}

fn fragment_is_anchor_candidate(node: &FragmentNode) -> bool {
  node.box_id().is_some()
    && !fragment_excludes_scroll_anchoring(node)
    && !fragment_is_non_atomic_inline(node)
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
  if !point_is_finite(start_origin) || !rect_is_finite(scrollport) {
    return None;
  }

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
        if let Some(rect) = checked_rect_for_node(frame.origin, frame.node) {
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
      let Some(origin) =
        checked_translate(frame.origin, Point::new(child.bounds.x(), child.bounds.y()))
      else {
        continue;
      };
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
  if !point_is_finite(start_origin) {
    return None;
  }

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
      let Some(origin) =
        checked_translate(frame.origin, Point::new(child.bounds.x(), child.bounds.y()))
      else {
        continue;
      };
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
  let (writing_mode, direction) =
    writing_mode_and_direction_from_style(tree.root.style.as_deref());
  super::viewport_rect_for_scroll_state(
    viewport_scroll,
    viewport,
    writing_mode,
    direction,
  )
}

fn element_scrollport(container: &FragmentNode, offset: Point) -> Rect {
  let viewport = Size::new(container.bounds.width(), container.bounds.height());
  let (writing_mode, direction) = writing_mode_and_direction_from_style(container.style.as_deref());
  super::viewport_rect_for_scroll_state(offset, viewport, writing_mode, direction)
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
) -> Option<(ScrollAnchor, f32)> {
  if !point_is_finite(start_origin) || !rect_is_finite(scrollport) {
    return None;
  }

  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a FragmentNode,
    origin: Point,
    next_child: usize,
    consider: bool,
    excluded: bool,
  }

  let mut best: Option<(ScrollAnchor, f32)> = None;
  let mut stack = vec![Frame {
    node: root,
    origin: start_origin,
    next_child: 0,
    consider: include_root,
    excluded: fragment_excludes_scroll_anchoring(root),
  }];

  while !stack.is_empty() {
    // Index-based traversal so we can scan ancestor frames without conflicting borrows.
    let last_idx = stack.len() - 1;
    let (node, origin, next_child, consider, excluded) = {
      let frame = &stack[last_idx];
      (frame.node, frame.origin, frame.next_child, frame.consider, frame.excluded)
    };

    if next_child == 0 && consider && !excluded && node.box_id() == Some(box_id) {
      // CSS Scroll Anchoring §2.2: "An anchor node can be any box except one for a non-atomic
      // inline." If the priority candidate is a non-atomic inline, walk up to the nearest ancestor
      // that is not a non-atomic inline (spec note).
      let promoted = if fragment_is_anchor_candidate(node) {
        Some((node, origin))
      } else if fragment_is_non_atomic_inline(node) {
        stack.iter().rev().find_map(|frame| {
          if frame.consider && !frame.excluded && fragment_is_anchor_candidate(frame.node) {
            Some((frame.node, frame.origin))
          } else {
            None
          }
        })
      } else {
        None
      };

      if let Some((candidate, candidate_origin)) = promoted {
        if let Some(rect) = checked_rect_for_node(candidate_origin, candidate) {
          if rect.intersects(scrollport) {
            let score = target_point.map_or(0.0, |p| {
              score_origin_relative_to_point(candidate_origin, rect, p)
            });
            if !score.is_finite() {
              // Ignore non-finite scores rather than letting NaN poison comparisons.
              // This can happen when upstream layout produces non-finite geometry.
              // Suppress this candidate and keep scanning.
            } else {
              let replace = match best {
                None => true,
                Some((_, best_score)) => score < best_score,
              };
              if replace {
                if let Some(candidate_box_id) = candidate.box_id() {
                  best = Some((
                    ScrollAnchor {
                      box_id: candidate_box_id,
                      origin: candidate_origin,
                    },
                    score,
                  ));
                }
              }
            }
          }
        }
      }
    }

    if excluded {
      stack.pop();
      continue;
    }

    if next_child < node.children.len() {
      let child_idx = next_child;
      stack[last_idx].next_child += 1;
      let child = &node.children[child_idx];
      let Some(child_origin) =
        checked_translate(origin, Point::new(child.bounds.x(), child.bounds.y()))
      else {
        continue;
      };
      stack.push(Frame {
        node: child,
        origin: child_origin,
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
  if !point_is_finite(root_origin) {
    return None;
  }
  let target_point = target_point.filter(|p| point_is_finite(*p));
  let best = best_anchor_for_box_id_in_subtree(
    &tree.root,
    box_id,
    scrollport,
    root_origin,
    false,
    target_point,
  )?;
  Some(best.0)
}

fn viewport_anchor_for_point(
  tree: &FragmentTree,
  scrollport: Rect,
  point: Point,
) -> Option<ScrollAnchor> {
  if !rect_is_finite(scrollport)
    || !point_is_finite(point)
    || !scrollport.contains_point(point)
  {
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
  if !point_is_finite(current_bounds.origin) {
    return None;
  }
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
    if !point_is_finite(current_bounds.origin) {
      return None;
    }
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
  // CSS Scroll Anchoring §2.2 step 1: `overflow-anchor:none` on the scrolling element disables
  // selecting an anchor for that scrolling box entirely.
  if fragment_excludes_scroll_anchoring(container) {
    return None;
  }
  let scrollport = element_scrollport(container, scroll);
  if !rect_is_finite(scrollport) {
    return None;
  }
  // Container local coordinates start at (0,0) for its children.
  for child in &container.children {
    let origin = Point::new(child.bounds.x(), child.bounds.y());
    if !point_is_finite(origin) {
      continue;
    }
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

  if tree
    .root
    .style
    .as_deref()
    .is_some_and(|style| style.overflow_anchor == OverflowAnchor::None)
  {
    snapshot.viewport = None;
  } else {
    let viewport_scroll = sanitize_point(scroll.viewport);
    snapshot.viewport =
      select_viewport_anchor_with_priority_candidate(tree, viewport_scroll, priority);
  }

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
  writing_mode: WritingMode,
  direction: Direction,
) -> Point {
  // Both `current_scroll` and the anchor positions originate from layout and can be polluted by
  // upstream NaN/±inf geometry. Treat any non-finite intermediate as a signal to suppress scroll
  // anchoring for this container (i.e. 0 adjustment).
  let current_scroll = sanitize_point(current_scroll);
  let Some(delta) = checked_point_sub(new_anchor_origin, prev_anchor.origin) else {
    return current_scroll;
  };
  let (x_sign, y_sign) = axis_signs_for_scroll_state(writing_mode, direction);
  let signed_delta = Point::new(x_sign * delta.x, y_sign * delta.y);
  let Some(adjusted) = checked_point_add(current_scroll, signed_delta) else {
    return current_scroll;
  };
  sanitize_point(adjusted)
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
  let viewport_for_units = new_tree.viewport_size();
  let mut next_scroll = scroll.clone();
  next_scroll.viewport = sanitize_point(next_scroll.viewport);
  next_scroll.viewport_delta = sanitize_point(next_scroll.viewport_delta);
  for offset in next_scroll.elements.values_mut() {
    *offset = sanitize_point(*offset);
  }
  for delta in next_scroll.elements_delta.values_mut() {
    *delta = sanitize_point(*delta);
  }
  let mut next_snapshot = ScrollAnchorSnapshot::default();
  let (viewport_writing_mode, viewport_direction) =
    writing_mode_and_direction_from_style(new_tree.root.style.as_deref());

  // Pre-clamp scroll offsets to the new layout's bounds so anchoring never leaves the scroll state
  // outside its valid range.
  let viewport_bounds = super::scroll_bounds_for_fragment(
    &new_tree.root,
    Point::ZERO,
    viewport_for_units,
    viewport_for_units,
    true,
    false,
  );
  next_scroll.viewport = viewport_bounds.clamp(next_scroll.viewport);
  let element_ids: Vec<usize> = next_scroll.elements.keys().copied().collect();
  for id in element_ids {
    let Some(container) = find_fragment_by_box_id(new_tree, id) else {
      continue;
    };
    let desired = sanitize_point(next_scroll.elements.get(&id).copied().unwrap_or(Point::ZERO));
    let bounds = super::scroll_bounds_for_fragment(
      container,
      Point::ZERO,
      container.bounds.size,
      viewport_for_units,
      false,
      false,
    );
    next_scroll.elements.insert(id, bounds.clamp(desired));
  }

  // Viewport scroll anchoring.
  // CSS Scroll Anchoring §2.2 step 1: `overflow-anchor:none` on the scrolling element disables
  // anchoring for that scroll container.
  if new_tree
    .root
    .style
    .as_deref()
    .is_some_and(|style| style.overflow_anchor == OverflowAnchor::None)
  {
    next_snapshot.viewport = None;
  } else if next_scroll.viewport == Point::ZERO {
    // CSS Scroll Anchoring §2.4 suppression trigger: if the scroll offset is zero, suppress.
    next_snapshot.viewport = select_viewport_anchor(new_tree, next_scroll.viewport);
  } else if let Some(prev_anchor) = previous.viewport.filter(|a| point_is_finite(a.origin)) {
    let root_origin = Point::new(new_tree.root.bounds.x(), new_tree.root.bounds.y());
    let new_origin = find_anchor_origin_in_subtree(
      &new_tree.root,
      prev_anchor.box_id,
      root_origin,
      false,
    );
    if let Some(new_origin) = new_origin {
      next_scroll.viewport = apply_one_adjustment(
        next_scroll.viewport,
        prev_anchor,
        new_origin,
        viewport_writing_mode,
        viewport_direction,
      );
      next_scroll.viewport = viewport_bounds.clamp(next_scroll.viewport);
      next_snapshot.viewport = Some(ScrollAnchor {
        box_id: prev_anchor.box_id,
        origin: new_origin,
      });
    } else {
      // Anchor missing/ineligible; no adjustment.
      next_snapshot.viewport = select_viewport_anchor(new_tree, next_scroll.viewport);
    }
  } else {
    next_snapshot.viewport = select_viewport_anchor(new_tree, next_scroll.viewport);
  }

  // Element scroll container anchoring.
  for (&container_id, &prev_anchor) in &previous.elements {
    if !point_is_finite(prev_anchor.origin) {
      continue;
    }
    let current_offset = next_scroll
      .elements
      .get(&container_id)
      .copied()
      .unwrap_or(Point::ZERO);
    let Some(container) = find_fragment_by_box_id(new_tree, container_id) else {
      continue;
    };
    let (writing_mode, direction) = writing_mode_and_direction_from_style(container.style.as_deref());

    // Suppress/disable if the current scroll offset is at the origin or the container opts out.
    if current_offset == Point::ZERO {
      if let Some(anchor) = select_element_anchor(new_tree, container_id, current_offset) {
        next_snapshot.elements.insert(container_id, anchor);
      }
      continue;
    }
    if container
      .style
      .as_deref()
      .is_some_and(|style| style.overflow_anchor == OverflowAnchor::None)
    {
      // Ensure we do not preserve any prior anchor for containers that disable anchoring.
      continue;
    }

    let new_origin =
      find_anchor_origin_in_subtree(container, prev_anchor.box_id, Point::ZERO, false);
    if let Some(new_origin) = new_origin {
      let adjusted = apply_one_adjustment(current_offset, prev_anchor, new_origin, writing_mode, direction);
      let bounds = super::scroll_bounds_for_fragment(
        container,
        Point::ZERO,
        container.bounds.size,
        viewport_for_units,
        false,
        false,
      );
      let adjusted = bounds.clamp(adjusted);
      if adjusted == Point::ZERO {
        next_scroll.elements.remove(&container_id);
      } else {
        next_scroll.elements.insert(container_id, adjusted);
      }
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
  next_scroll
    .elements
    .retain(|_, offset| *offset != Point::ZERO);
  for (&container_id, &offset) in &next_scroll.elements {
    if next_snapshot.elements.contains_key(&container_id) {
      continue;
    }
    if let Some(anchor) = select_element_anchor(new_tree, container_id, sanitize_point(offset)) {
      next_snapshot.elements.insert(container_id, anchor);
    }
  }

  next_scroll.update_deltas_from(scroll);

  (next_scroll, next_snapshot)
}

/// Apply scroll anchoring adjustments, integrating CSS Scroll Snap re-snapping for snapped scrollers.
///
/// CSS Scroll Anchoring (§2 "Scroll anchoring adjustments") notes that snapped scrollers should not
/// end up between snap points: after applying anchoring, the UA re-snaps the scroll position. This
/// helper implements that behaviour by:
/// - Detecting which scrolling boxes were snapped in the previous layout (via `apply_scroll_snap`).
/// - Applying scroll anchoring to the new layout.
/// - Re-running scroll snap for the *previously snapped* scrolling boxes and accepting the snapped
///   result.
///
/// The returned scroll state has `viewport_delta`/`elements_delta` recomputed relative to `scroll`.
pub(crate) fn apply_scroll_anchoring_with_scroll_snap(
  prev_tree: &mut FragmentTree,
  new_tree: &mut FragmentTree,
  scrollport_viewport: Size,
  scroll: &ScrollState,
  viewport_priority: Option<ScrollAnchoringPriorityCandidate>,
) -> ScrollState {
  // Determine which containers were snapped in the previous layout.
  prev_tree.ensure_scroll_metadata();
  // Ensure overflow metadata is available for clamping in the new layout even when no scroll snap
  // containers are currently snapped.
  new_tree.ensure_scroll_metadata();
  let epsilon = 0.1;
  let mut snapped_viewport = false;
  let mut snapped_elements: HashSet<usize> = HashSet::new();

  if let Some(prev_metadata) = prev_tree.scroll_metadata.as_ref() {
    let snapped_prev = super::apply_scroll_snap_from_metadata(prev_metadata, scroll).state;

    for container in &prev_metadata.containers {
      if !container.snap_x && !container.snap_y {
        continue;
      }
      if container.uses_viewport_scroll {
        if approx_eq_point(scroll.viewport, snapped_prev.viewport, epsilon) {
          snapped_viewport = true;
        }
        continue;
      }
      let Some(id) = container.box_id else {
        continue;
      };
      let Some(current) = scroll.elements.get(&id) else {
        continue;
      };
      let Some(snapped) = snapped_prev.elements.get(&id) else {
        continue;
      };
      if approx_eq_point(*current, *snapped, epsilon) {
        snapped_elements.insert(id);
      }
    }
  }

  let mut next_scroll = apply_scroll_anchoring_between_trees(
    prev_tree,
    new_tree,
    scroll,
    scrollport_viewport,
    viewport_priority,
  );

  if snapped_viewport || !snapped_elements.is_empty() {
    let snapped_after = super::apply_scroll_snap(new_tree, &next_scroll).state;
    if snapped_viewport {
      next_scroll.viewport = snapped_after.viewport;
    }
    for id in snapped_elements {
      if let Some(offset) = snapped_after.elements.get(&id).copied() {
        next_scroll.elements.insert(id, offset);
      }
    }
  }

  // Mirror paint-time viewport clamping so callers that only flush layout (no paint) still see a
  // stable scroll state.
  next_scroll.viewport = sanitize_point(next_scroll.viewport);
  for offset in next_scroll.elements.values_mut() {
    *offset = sanitize_point(*offset);
  }
  next_scroll.viewport = super::viewport_scroll_bounds(&new_tree.root, scrollport_viewport)
    .clamp(next_scroll.viewport);

  // Mirror paint-time element scroll clamping so layout-only flushes can't leave element scroll
  // offsets outside their new bounds (which would later be corrected during paint).
  let element_ids: Vec<usize> = next_scroll.elements.keys().copied().collect();
  for id in element_ids {
    let Some(container) = find_fragment_by_box_id(new_tree, id) else {
      continue;
    };
    let desired = sanitize_point(next_scroll.elements.get(&id).copied().unwrap_or(Point::ZERO));
    let bounds = super::scroll_bounds_for_fragment(
      container,
      Point::ZERO,
      container.bounds.size,
      scrollport_viewport,
      false,
      false,
    );
    next_scroll.elements.insert(id, bounds.clamp(desired));
  }

  next_scroll.update_deltas_from(scroll);
  next_scroll
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAnchorContainer {
  Viewport,
  Element(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollAnchorContainerEntry {
  container: ScrollAnchorContainer,
  depth: usize,
}

fn is_element_scroll_container(style: &ComputedStyle) -> bool {
  matches!(style.overflow_x, Overflow::Hidden | Overflow::Scroll | Overflow::Auto)
    || matches!(style.overflow_y, Overflow::Hidden | Overflow::Scroll | Overflow::Auto)
}

fn collect_scroll_anchor_containers(node: &FragmentNode, depth: usize, out: &mut HashMap<usize, usize>) {
  let mut depth_for_children = depth;
  if let Some(style) = node.style.as_deref() {
    if is_element_scroll_container(style) {
      if let Some(id) = node.box_id() {
        let next_depth = depth.saturating_add(1);
        out
          .entry(id)
          .and_modify(|stored| *stored = (*stored).max(next_depth))
          .or_insert(next_depth);
      }
      depth_for_children = depth.saturating_add(1);
    }
  }

  for child in node.children.iter() {
    collect_scroll_anchor_containers(child, depth_for_children, out);
  }
}

fn find_fragment_with_box_id_and_origin<'a>(
  tree: &'a FragmentTree,
  box_id: usize,
) -> Option<(&'a FragmentNode, Point)> {
  let mut stack: Vec<(&'a FragmentNode, Point)> = Vec::new();
  stack.push((&tree.root, tree.root.bounds.origin));
  for root in &tree.additional_fragments {
    stack.push((root, root.bounds.origin));
  }

  while let Some((node, origin)) = stack.pop() {
    if node.box_id() == Some(box_id) {
      return Some((node, origin));
    }
    for child in node.children.iter().rev() {
      let child_origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
      stack.push((child, child_origin));
    }
  }

  None
}

fn scrollport_rect_in_page(
  tree: &FragmentTree,
  scroll: &ScrollState,
  viewport: Size,
  container: ScrollAnchorContainer,
) -> Option<Rect> {
  match container {
    ScrollAnchorContainer::Viewport => Some(Rect::from_xywh(
      scroll.viewport.x,
      scroll.viewport.y,
      viewport.width,
      viewport.height,
    )),
    ScrollAnchorContainer::Element(box_id) => {
      let (node, origin) = find_fragment_with_box_id_and_origin(tree, box_id)?;
      let style = node.style.as_deref()?;
      Some(super::scrollport_rect_for_fragment(node, style).translate(origin))
    }
  }
}

fn anchor_selection_point(scrollport: Rect) -> Option<Point> {
  let w = scrollport.width().max(0.0);
  let h = scrollport.height().max(0.0);
  if w <= 0.0 || h <= 0.0 {
    return None;
  }

  let epsilon_x = 1.0_f32.min(w * 0.5);
  let epsilon_y = 1.0_f32.min(h * 0.5);
  Some(Point::new(
    scrollport.min_x() + epsilon_x,
    scrollport.min_y() + epsilon_y,
  ))
}

fn path_nodes<'a>(
  tree: &'a FragmentTree,
  root: HitTestRoot,
  path: &[usize],
) -> Option<Vec<&'a FragmentNode>> {
  let mut out = Vec::with_capacity(path.len().saturating_add(1));
  let mut current = match root {
    HitTestRoot::Root => &tree.root,
    HitTestRoot::Additional(idx) => tree.additional_fragments.get(idx)?,
  };
  out.push(current);
  for &idx in path {
    current = current.children.get(idx)?;
    out.push(current);
  }
  Some(out)
}

fn select_anchor_box_id(
  tree: &FragmentTree,
  container: ScrollAnchorContainer,
  scrollport: Rect,
) -> Option<usize> {
  let point = anchor_selection_point(scrollport)?;
  let (root, path) = tree.hit_test_path(point)?;
  let nodes = path_nodes(tree, root, &path)?;

  // Respect `overflow-anchor:none`: once an excluded node appears on the hit-test path, everything
  // below it is part of an excluded subtree. Select the deepest candidate *above* the excluded node.
  let eligible_end = nodes
    .iter()
    .position(|node| fragment_excludes_scroll_anchoring(node))
    .unwrap_or(nodes.len());

  let start = match container {
    ScrollAnchorContainer::Viewport => 0,
    ScrollAnchorContainer::Element(container_id) => {
      let pos = nodes.iter().rposition(|node| node.box_id() == Some(container_id))?;
      pos.saturating_add(1)
    }
  };

  if eligible_end <= start {
    return None;
  }

  nodes
    .iter()
    .take(eligible_end)
    .skip(start)
    .rev()
    .filter_map(|node| node.box_id())
    .find(|&id| match container {
      ScrollAnchorContainer::Viewport => true,
      ScrollAnchorContainer::Element(container_id) => id != container_id,
    })
}

fn find_fragment_rect_for_box_id_in_scrollport(
  tree: &FragmentTree,
  box_id: usize,
  scrollport: Rect,
) -> Option<Rect> {
  let mut stack: Vec<(&FragmentNode, Point)> = Vec::new();
  stack.push((&tree.root, tree.root.bounds.origin));
  for root in &tree.additional_fragments {
    stack.push((root, root.bounds.origin));
  }

  let mut best: Option<(Rect, (f32, f32))> = None;

  while let Some((node, origin)) = stack.pop() {
    if node.box_id() == Some(box_id) && !fragment_excludes_scroll_anchoring(node) {
      let rect = Rect::from_xywh(origin.x, origin.y, node.bounds.width(), node.bounds.height());
      if rect.intersects(scrollport)
        && rect.min_x().is_finite()
        && rect.min_y().is_finite()
        && rect.width().is_finite()
        && rect.height().is_finite()
      {
        let key_y = rect.min_y().max(scrollport.min_y());
        let key_x = rect.min_x().max(scrollport.min_x());
        let key = (key_y, key_x);
        let replace = best.map(|(_, best_key)| key < best_key).unwrap_or(true);
        if replace {
          best = Some((rect, key));
        }
      }
    }

    for child in node.children.iter().rev() {
      let child_origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
      stack.push((child, child_origin));
    }
  }

  best.map(|(rect, _)| rect)
}

fn anchor_relative_position(anchor: Rect, scrollport: Rect) -> Option<Point> {
  let anchor_min = Point::new(anchor.min_x(), anchor.min_y());
  let scrollport_min = Point::new(scrollport.min_x(), scrollport.min_y());
  let rel = Point::new(anchor_min.x - scrollport_min.x, anchor_min.y - scrollport_min.y);
  (rel.x.is_finite() && rel.y.is_finite()).then_some(rel)
}

/// Adjust scroll offsets across a relayout using scroll anchoring, handling nested scroll containers.
///
/// This helper differs from [`apply_scroll_anchoring`] in two key ways:
/// - It operates directly on a *pair* of fragment trees (before/after layout) rather than requiring a
///   long-lived [`ScrollAnchorSnapshot`].
/// - It processes element scroll containers + the viewport from *innermost to outermost*, updating
///   the scroll state as it goes. This ensures layout shifts confined to an inner scroll container
///   do not induce scroll anchoring adjustments on ancestor containers.
///
/// The returned [`ScrollState`] includes fresh `*_delta` fields derived from the offset changes.
pub fn apply_scroll_anchoring_between_trees(
  prev: &FragmentTree,
  next: &FragmentTree,
  old_scroll: &ScrollState,
  viewport: Size,
  viewport_priority: Option<ScrollAnchoringPriorityCandidate>,
) -> ScrollState {
  // Determine scroll-container nesting depth from the previous layout tree so processing order is
  // stable across the relayout.
  let mut depths: HashMap<usize, usize> = HashMap::new();
  collect_scroll_anchor_containers(&prev.root, 0, &mut depths);
  for root in &prev.additional_fragments {
    collect_scroll_anchor_containers(root, 0, &mut depths);
  }

  // Only element scroll containers with non-zero offsets participate (mirrors canonical scroll
  // state representation).
  let mut ordered: Vec<ScrollAnchorContainerEntry> = old_scroll
    .elements
    .keys()
    .filter_map(|&id| depths.get(&id).copied().map(|depth| ScrollAnchorContainerEntry {
      container: ScrollAnchorContainer::Element(id),
      depth,
    }))
    .collect();
  ordered.push(ScrollAnchorContainerEntry {
    container: ScrollAnchorContainer::Viewport,
    depth: 0,
  });

  ordered.sort_by(|a, b| {
    b.depth.cmp(&a.depth).then_with(|| match (a.container, b.container) {
      (ScrollAnchorContainer::Viewport, ScrollAnchorContainer::Viewport) => Ordering::Equal,
      (ScrollAnchorContainer::Viewport, ScrollAnchorContainer::Element(_)) => Ordering::Greater,
      (ScrollAnchorContainer::Element(_), ScrollAnchorContainer::Viewport) => Ordering::Less,
      (ScrollAnchorContainer::Element(a_id), ScrollAnchorContainer::Element(b_id)) => a_id.cmp(&b_id),
    })
  });

  // The input fragment trees are in an unscrolled coordinate space; apply element scroll offsets so
  // hit testing and anchor geometry match what was actually visible.
  let mut prev_scrolled = prev.clone();
  super::apply_scroll_offsets(&mut prev_scrolled, old_scroll);

  let (viewport_writing_mode, viewport_direction) =
    writing_mode_and_direction_from_style(next.root.style.as_deref());
  let mut state = ScrollState::from_parts(old_scroll.viewport, old_scroll.elements.clone());

  for entry in ordered {
    let container = entry.container;
    let Some(scrollport_old) = scrollport_rect_in_page(&prev_scrolled, old_scroll, viewport, container) else {
      continue;
    };
    let anchor_id = match container {
      ScrollAnchorContainer::Viewport => viewport_priority
        .and_then(|candidate| {
          viewport_anchor_for_priority_candidate(&prev_scrolled, scrollport_old, candidate)
        })
        .map(|anchor| anchor.box_id)
        .or_else(|| select_anchor_box_id(&prev_scrolled, container, scrollport_old)),
      ScrollAnchorContainer::Element(_) => select_anchor_box_id(&prev_scrolled, container, scrollport_old),
    };
    let Some(anchor_id) = anchor_id else {
      continue;
    };
    let Some(anchor_old) =
      find_fragment_rect_for_box_id_in_scrollport(&prev_scrolled, anchor_id, scrollport_old)
    else {
      continue;
    };
    let Some(old_rel) = anchor_relative_position(anchor_old, scrollport_old) else {
      continue;
    };

    let mut next_scrolled = next.clone();
    super::apply_scroll_offsets(&mut next_scrolled, &state);

    let Some(scrollport_new) = scrollport_rect_in_page(&next_scrolled, &state, viewport, container) else {
      continue;
    };
    let Some(anchor_new) =
      find_fragment_rect_for_box_id_in_scrollport(&next_scrolled, anchor_id, scrollport_new)
    else {
      continue;
    };
    let Some(new_rel) = anchor_relative_position(anchor_new, scrollport_new) else {
      continue;
    };

    let Some(delta) = checked_point_sub(new_rel, old_rel) else {
      continue;
    };
    if delta == Point::ZERO {
      continue;
    }

    let (writing_mode, direction) = match container {
      ScrollAnchorContainer::Viewport => (viewport_writing_mode, viewport_direction),
      ScrollAnchorContainer::Element(id) => {
        let style = find_fragment_by_box_id(next, id).and_then(|node| node.style.as_deref());
        writing_mode_and_direction_from_style(style)
      }
    };
    let (x_sign, y_sign) = axis_signs_for_scroll_state(writing_mode, direction);
    let delta = Point::new(x_sign * delta.x, y_sign * delta.y);

    match container {
      ScrollAnchorContainer::Viewport => {
        let Some(updated) = checked_point_add(state.viewport, delta) else {
          continue;
        };
        state.viewport = sanitize_point(updated);
      }
      ScrollAnchorContainer::Element(id) => {
        let current = state.elements.get(&id).copied().unwrap_or(Point::ZERO);
        let Some(updated) = checked_point_add(current, delta) else {
          continue;
        };
        let updated = sanitize_point(updated);
        if updated == Point::ZERO {
          state.elements.remove(&id);
        } else {
          state.elements.insert(id, updated);
        }
      }
    }
  }

  state.update_deltas_from(old_scroll);
  state
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Size;
  use crate::tree::fragment_tree::{FragmentContent, FragmentTree};
  use crate::ComputedStyle;
  use std::sync::Arc;

  fn style_with_display(display: Display) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = display;
    Arc::new(style)
  }

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

  #[test]
  fn anchor_selection_skips_non_atomic_inline_fragments() {
    // The viewport scrollport is 0..100 in Y. The line spans 90..110 so it's partially visible. It
    // contains an inline fragment first and a block fragment second; without excluding non-atomic
    // inline boxes, the inline fragment would be chosen as the anchor node.
    let inline = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Inline {
        box_id: Some(2),
        fragment_index: 0,
      },
      vec![],
      style_with_display(Display::Inline),
    );
    let block = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Block { box_id: Some(1) },
      vec![],
      style_with_display(Display::Block),
    );
    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 90.0, 50.0, 20.0),
      0.0,
      vec![inline, block],
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![line]);
    let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));

    let anchor = select_viewport_anchor(&tree, Point::ZERO).expect("expected an anchor selection");
    assert_eq!(
      anchor.box_id, 1,
      "expected the block fragment (box_id=1) to be selected, got {anchor:?}"
    );
  }

  #[test]
  fn priority_candidate_non_atomic_inline_is_promoted_to_ancestor() {
    // Priority candidates (e.g. focused elements) that land on non-atomic inline fragments must be
    // promoted to the nearest ancestor that is not a non-atomic inline (§2.2 note).
    let inline = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Inline {
        box_id: Some(2),
        fragment_index: 0,
      },
      vec![],
      style_with_display(Display::Inline),
    );
    let target_block = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 90.0, 50.0, 20.0),
      FragmentContent::Block { box_id: Some(1) },
      vec![inline],
      style_with_display(Display::Block),
    );

    // Place a different anchor candidate earlier in traversal order so we can ensure the priority
    // candidate is actually used (i.e. we don't just fall back to default anchor selection).
    let earlier_block = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Block { box_id: Some(10) },
      vec![],
      style_with_display(Display::Block),
    );

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      vec![earlier_block, target_block],
    );
    let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
    let scroll = ScrollState::with_viewport(Point::ZERO);

    let snapshot = capture_scroll_anchors_with_priority(
      &tree,
      &scroll,
      Some(ScrollAnchoringPriorityCandidate::BoxId {
        box_id: 2,
        point: None,
      }),
    );
    let anchor = snapshot.viewport.expect("expected a viewport anchor selection");
    assert_eq!(anchor.box_id, 1);
  }
}
