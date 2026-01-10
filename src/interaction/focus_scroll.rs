use crate::geometry::{Point, Rect, Size};
use crate::scroll::{build_scroll_chain, build_scroll_chain_with_root_mode, ScrollBounds, ScrollChainState, ScrollState};
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree, HitTestRoot};
use rustc_hash::FxHashSet;

use super::fragment_geometry::absolute_bounds_for_box_id;

const DEFAULT_FOCUS_SCROLL_PADDING_CSS: f32 = 8.0;

fn sanitize_point(point: Point) -> Point {
  Point::new(
    if point.x.is_finite() { point.x } else { 0.0 },
    if point.y.is_finite() { point.y } else { 0.0 },
  )
}

fn scrollport_size_for_state(state: &ScrollChainState<'_>) -> Size {
  let reservation = state.container.scrollbar_reservation;
  let width = state.viewport.width - reservation.left - reservation.right;
  let height = state.viewport.height - reservation.top - reservation.bottom;
  Size::new(
    if width.is_finite() { width.max(0.0) } else { 0.0 },
    if height.is_finite() { height.max(0.0) } else { 0.0 },
  )
}

fn clamp_padding(padding: f32, extent: f32) -> f32 {
  if !padding.is_finite() || !extent.is_finite() || extent <= 0.0 {
    return 0.0;
  }
  padding.max(0.0).min(extent * 0.5)
}

fn adjust_scroll_axis_nearest(
  current_scroll: f32,
  target_min: f32,
  target_max: f32,
  viewport_extent: f32,
  padding: f32,
) -> f32 {
  if !current_scroll.is_finite()
    || !target_min.is_finite()
    || !target_max.is_finite()
    || !viewport_extent.is_finite()
    || viewport_extent <= 0.0
  {
    return current_scroll;
  }
  let (target_min, target_max) = if target_max < target_min {
    (target_max, target_min)
  } else {
    (target_min, target_max)
  };

  let padding = clamp_padding(padding, viewport_extent);
  let padded_start = padding;
  let padded_end = (viewport_extent - padding).max(padded_start);

  // Visible coordinates of the target rect in scrollport space.
  let start = target_min - current_scroll;
  let end = target_max - current_scroll;

  if start < padded_start && end <= padded_end {
    // Target is above/left of the padded viewport; scroll backwards so its start edge is visible.
    current_scroll + (start - padded_start)
  } else if end > padded_end && start >= padded_start {
    // Target is below/right of the padded viewport; scroll forwards so its end edge is visible.
    current_scroll + (end - padded_end)
  } else {
    // Already fully visible (or too large to fit); do not scroll.
    current_scroll
  }
}

fn scroll_to_reveal_rect(
  current_scroll: Point,
  bounds: ScrollBounds,
  target: Rect,
  viewport: Size,
  padding: f32,
) -> Point {
  let current_scroll = sanitize_point(current_scroll);
  let viewport = Size::new(
    if viewport.width.is_finite() { viewport.width.max(0.0) } else { 0.0 },
    if viewport.height.is_finite() { viewport.height.max(0.0) } else { 0.0 },
  );

  // Focus-driven auto-scroll adjusts both axes, but we clamp horizontal scrolling to non-negative
  // offsets. The renderer's scroll bounds can become negative when content is positioned off-screen
  // to the left (e.g. `left:-9999px` hacks for visually-hidden form controls). Browsers generally
  // do not scroll to negative `scrollLeft` values, and doing so would cause extremely surprising
  // page jumps during focus traversal.
  let horizontal_bounds = ScrollBounds {
    min_x: bounds.min_x.max(0.0),
    max_x: bounds.max_x.max(0.0),
    ..bounds
  };

  let next_x = adjust_scroll_axis_nearest(
    current_scroll.x,
    target.min_x(),
    target.max_x(),
    viewport.width,
    padding,
  );
  let next_y = adjust_scroll_axis_nearest(
    current_scroll.y,
    target.min_y(),
    target.max_y(),
    viewport.height,
    padding,
  );
  horizontal_bounds.clamp(Point::new(next_x, next_y))
}

fn collect_box_ids_for_styled_node(box_tree: &BoxTree, styled_node_id: usize) -> FxHashSet<usize> {
  let mut out: FxHashSet<usize> = FxHashSet::default();
  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      out.insert(node.id);
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

fn union_bounds_for_box_ids(fragment_tree: &FragmentTree, box_ids: &FxHashSet<usize>) -> Option<Rect> {
  let mut bounds: Option<Rect> = None;
  for id in box_ids.iter() {
    let Some(rect) = absolute_bounds_for_box_id(fragment_tree, *id) else {
      continue;
    };
    bounds = Some(match bounds {
      Some(existing) => existing.union(rect),
      None => rect,
    });
  }
  bounds
}

fn find_fragment_path_within_root(root: &FragmentNode, box_ids: &FxHashSet<usize>) -> Option<Vec<usize>> {
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
    if frame.next_child == 0 {
      if frame
        .node
        .box_id()
        .is_some_and(|box_id| box_ids.contains(&box_id))
      {
        return Some(path.clone());
      }
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
      if !path.is_empty() {
        path.pop();
      }
    }
  }

  None
}

fn find_fragment_path_for_box_ids(fragment_tree: &FragmentTree, box_ids: &FxHashSet<usize>) -> Option<(HitTestRoot, Vec<usize>)> {
  if let Some(path) = find_fragment_path_within_root(&fragment_tree.root, box_ids) {
    return Some((HitTestRoot::Root, path));
  }
  for (idx, root) in fragment_tree.additional_fragments.iter().enumerate() {
    if let Some(path) = find_fragment_path_within_root(root, box_ids) {
      return Some((HitTestRoot::Additional(idx), path));
    }
  }
  None
}

fn apply_focus_scroll_chain(
  chain: &mut [ScrollChainState<'_>],
  target_bounds: Rect,
  last_is_viewport: bool,
  scroll_state: &ScrollState,
) -> ScrollState {
  let chain_len = chain.len();
  let mut next = scroll_state.clone();

  // Seed current scroll offsets into the chain.
  for (idx, state) in chain.iter_mut().enumerate() {
    if last_is_viewport && idx == chain_len.saturating_sub(1) {
      state.scroll = sanitize_point(scroll_state.viewport);
    } else if let Some(id) = state.container.box_id() {
      state.scroll = sanitize_point(scroll_state.element_offset(id));
    } else {
      state.scroll = Point::ZERO;
    }
  }

  let mut descendant_scroll = Point::ZERO;
  for (idx, state) in chain.iter_mut().enumerate() {
    let is_viewport = last_is_viewport && idx == chain_len.saturating_sub(1);
    let can_scroll = is_viewport || state.container.box_id().is_some();
    let origin = if is_viewport { Point::ZERO } else { state.origin };

    let target_local = target_bounds
      .translate(Point::new(-origin.x, -origin.y))
      .translate(Point::new(-descendant_scroll.x, -descendant_scroll.y));

    let viewport = scrollport_size_for_state(state);
    if can_scroll {
      state.scroll = scroll_to_reveal_rect(
        state.scroll,
        state.bounds,
        target_local,
        viewport,
        DEFAULT_FOCUS_SCROLL_PADDING_CSS,
      );
      descendant_scroll = descendant_scroll.translate(state.scroll);
    }

    // If we can't represent scroll for this container (no box id), keep it at zero and do not
    // incorporate it into descendant offsets for outer containers.
  }

  // Write scroll offsets back into the `ScrollState`.
  for (idx, state) in chain.iter().enumerate() {
    if last_is_viewport && idx == chain_len.saturating_sub(1) {
      next.viewport = state.scroll;
    } else if let Some(id) = state.container.box_id() {
      if state.scroll == Point::ZERO {
        next.elements.remove(&id);
      } else {
        next.elements.insert(id, state.scroll);
      }
    }
  }

  next.elements.retain(|_, offset| *offset != Point::ZERO);
  next
}

/// Compute a suggested scroll state update when focus moves to `focused_node_id`.
///
/// This mirrors basic browser UX: when focus changes (Tab traversal or click focus), the viewport
/// and any overflow scroll containers are scrolled just enough to bring the focused element into
/// view, with a small padding.
pub fn scroll_state_for_focus(
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
  focused_node_id: usize,
) -> Option<ScrollState> {
  let box_ids = collect_box_ids_for_styled_node(box_tree, focused_node_id);
  if box_ids.is_empty() {
    return None;
  }
  let Some(target_bounds) = union_bounds_for_box_ids(fragment_tree, &box_ids) else {
    return None;
  };
  let Some((root_kind, path)) = find_fragment_path_for_box_ids(fragment_tree, &box_ids) else {
    return None;
  };

  let viewport_size = fragment_tree.viewport_size();

  let mut next = match root_kind {
    HitTestRoot::Root => {
      let mut chain = build_scroll_chain(&fragment_tree.root, viewport_size, &path);
      if chain.is_empty() {
        return None;
      }
      apply_focus_scroll_chain(&mut chain, target_bounds, true, scroll_state)
    }
    HitTestRoot::Additional(idx) => {
      let Some(root) = fragment_tree.additional_fragments.get(idx) else {
        return None;
      };
      let mut chain = build_scroll_chain_with_root_mode(root, root.bounds.size, &path, false);
      let mut next = if chain.is_empty() {
        scroll_state.clone()
      } else {
        apply_focus_scroll_chain(&mut chain, target_bounds, false, scroll_state)
      };

      // Ensure the focused target is also visible in the document viewport.
      let viewport_chain = build_scroll_chain(&fragment_tree.root, viewport_size, &[]);
      let Some(viewport_state) = viewport_chain.last() else {
        return Some(next);
      };

      let element_shift = chain.iter().fold(Point::ZERO, |acc, state| {
        if state.container.box_id().is_some() {
          acc.translate(state.scroll)
        } else {
          acc
        }
      });

      let target_in_viewport_space = target_bounds.translate(Point::new(-element_shift.x, -element_shift.y));
      let viewport_scrollport = scrollport_size_for_state(viewport_state);

      next.viewport = scroll_to_reveal_rect(
        next.viewport,
        viewport_state.bounds,
        target_in_viewport_space,
        viewport_scrollport,
        DEFAULT_FOCUS_SCROLL_PADDING_CSS,
      );

      next
    }
  };

  next.viewport = sanitize_point(next.viewport);
  if next != *scroll_state {
    Some(next)
  } else {
    None
  }
}
