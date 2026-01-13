//! AccessKit helpers for exposing FastRender-scrolled content to assistive tech.
//!
//! This module is intentionally small and self-contained: it provides:
//! - A minimal AccessKit tree builder for scroll containers (viewport + element scrollers), and
//! - Logic for applying AccessKit scroll actions to a [`crate::scroll::ScrollState`].
//!
//! The higher-level UI integration (winit adapter plumbing, rerender scheduling, etc.) lives in the
//! windowed browser UI stack. This module focuses on the renderer-facing pieces that need access to
//! layout/style/scroll state.

use std::collections::HashMap;
use std::num::NonZeroU128;

use accesskit::{
  Action, ActionRequest, Node, NodeBuilder, NodeClassSet, NodeId, Rect, Tree, TreeUpdate,
};

use crate::geometry::{Point, Size};
use crate::scroll::{ScrollBounds, ScrollState};
use crate::style::types::Overflow;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};

/// The AccessKit node id reserved for the root scroll container (viewport scroll).
///
/// All element scroll containers are assigned `NodeId = box_id + 1`, so the root id must not
/// collide with any real `box_id` (which start at 1).
pub const ROOT_SCROLL_CONTAINER_ID: NodeId = NodeId(unsafe { NonZeroU128::new_unchecked(1) });

const ELEMENT_SCROLL_CONTAINER_ID_OFFSET: u128 = 1;

/// Convert an element scroll container box id into an AccessKit node id.
pub fn node_id_for_scroll_box_id(box_id: usize) -> NodeId {
  // Box IDs are assigned from 1, but be defensive and still allow 0 by mapping it to `2`.
  let raw = u128::from(box_id as u64).saturating_add(ELEMENT_SCROLL_CONTAINER_ID_OFFSET);
  NodeId(NonZeroU128::new(raw.max(2)).expect("offset ensures non-zero")) // fastrender-allow-unwrap
}

fn scroll_box_id_for_node_id(node_id: NodeId) -> Option<usize> {
  let raw = node_id.0.get();
  if raw <= ROOT_SCROLL_CONTAINER_ID.0.get() {
    return None;
  }
  let box_raw = raw.saturating_sub(ELEMENT_SCROLL_CONTAINER_ID_OFFSET);
  usize::try_from(box_raw).ok()
}

fn is_overflow_scrollable(overflow: Overflow) -> bool {
  // CSS Overflow 3: `overflow: hidden` still establishes a scroll container (scrollbars are simply
  // suppressed). Assistive tech should still be able to drive scroll actions for these containers.
  matches!(
    overflow,
    Overflow::Auto | Overflow::Scroll | Overflow::Hidden
  )
}

fn fragment_is_scroll_container(node: &FragmentNode) -> bool {
  let Some(style) = node.style.as_deref() else {
    return false;
  };
  is_overflow_scrollable(style.overflow_x) || is_overflow_scrollable(style.overflow_y)
}

#[derive(Debug, Clone, Copy)]
struct ScrollContainerInfo {
  bounds: ScrollBounds,
  scrollport: Size,
}

fn sanitize_nonneg_f32(value: f32) -> f32 {
  if value.is_finite() {
    value.max(0.0)
  } else {
    0.0
  }
}

fn sanitize_point_nonneg(point: Point) -> Point {
  Point::new(sanitize_nonneg_f32(point.x), sanitize_nonneg_f32(point.y))
}

fn clamp_point_to_bounds(point: Point, bounds: ScrollBounds) -> Point {
  let clamp_axis = |value: f32, min: f32, max: f32| -> f32 {
    if !value.is_finite() || !min.is_finite() || !max.is_finite() {
      return sanitize_nonneg_f32(value);
    }
    if min > max {
      return sanitize_nonneg_f32(value);
    }
    value.clamp(min, max)
  };
  Point::new(
    clamp_axis(point.x, bounds.min_x, bounds.max_x),
    clamp_axis(point.y, bounds.min_y, bounds.max_y),
  )
}

fn accesskit_rect_from_xywh(x: f32, y: f32, w: f32, h: f32) -> Rect {
  let x0 = x as f64;
  let y0 = y as f64;
  let x1 = (x + w.max(0.0)) as f64;
  let y1 = (y + h.max(0.0)) as f64;
  Rect::new(x0, y0, x1, y1)
}

fn scrollport_size_for_root(root_fragment: &FragmentNode, viewport: Size) -> Size {
  let reservation = root_fragment.scrollbar_reservation;
  let reserve_left = sanitize_nonneg_f32(reservation.left);
  let reserve_right = sanitize_nonneg_f32(reservation.right);
  let reserve_top = sanitize_nonneg_f32(reservation.top);
  let reserve_bottom = sanitize_nonneg_f32(reservation.bottom);
  Size::new(
    (viewport.width - reserve_left - reserve_right).max(0.0),
    (viewport.height - reserve_top - reserve_bottom).max(0.0),
  )
}

fn scroll_container_info_for_root(tree: &FragmentTree, viewport: Size) -> ScrollContainerInfo {
  let bounds = crate::scroll::scroll_bounds_for_fragment(
    &tree.root,
    Point::ZERO,
    viewport,
    viewport,
    true,
    false,
  );
  ScrollContainerInfo {
    bounds,
    scrollport: scrollport_size_for_root(&tree.root, viewport),
  }
}

fn scroll_container_info_for_box_id(
  tree: &FragmentTree,
  root_viewport: Size,
  box_id: usize,
) -> Option<ScrollContainerInfo> {
  struct Frame<'a> {
    node: &'a FragmentNode,
    has_fixed_cb_ancestor: bool,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      has_fixed_cb_ancestor: false,
    });
  }
  stack.push(Frame {
    node: &tree.root,
    has_fixed_cb_ancestor: false,
  });

  while let Some(frame) = stack.pop() {
    let node = frame.node;
    let has_fixed_cb_ancestor = frame.has_fixed_cb_ancestor;

    if node.box_id() == Some(box_id) && fragment_is_scroll_container(node) {
      let viewport = node.bounds.size;
      let bounds = crate::scroll::scroll_bounds_for_fragment(
        node,
        Point::ZERO,
        viewport,
        root_viewport,
        false,
        has_fixed_cb_ancestor,
      );

      let scrollport = node
        .style
        .as_deref()
        .map(|style| crate::scroll::scrollport_rect_for_fragment(node, style).size)
        .unwrap_or(viewport);

      return Some(ScrollContainerInfo { bounds, scrollport });
    }

    let establishes_fixed_cb = node
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_fixed_containing_block());
    let child_fixed = has_fixed_cb_ancestor || establishes_fixed_cb;

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        has_fixed_cb_ancestor: child_fixed,
      });
    }
  }

  None
}

fn set_scroll_properties(
  builder: &mut NodeBuilder,
  current: Point,
  bounds: ScrollBounds,
  scrollport: Size,
) {
  let current = sanitize_point_nonneg(current);
  let current = clamp_point_to_bounds(current, bounds);

  builder.set_scroll_x(current.x as f64);
  builder.set_scroll_y(current.y as f64);
  builder.set_scroll_x_min(bounds.min_x as f64);
  builder.set_scroll_x_max(bounds.max_x as f64);
  builder.set_scroll_y_min(bounds.min_y as f64);
  builder.set_scroll_y_max(bounds.max_y as f64);
  let _ = scrollport; // AccessKit 0.11 does not expose page-size/viewport-size fields.

  // Advertise supported actions so assistive tech can drive scrolling.
  for action in [
    Action::ScrollUp,
    Action::ScrollDown,
    Action::ScrollLeft,
    Action::ScrollRight,
    Action::ScrollForward,
    Action::ScrollBackward,
    Action::ScrollToPoint,
    Action::SetScrollOffset,
  ] {
    builder.add_action(action);
  }
}

fn is_scroll_action(action: Action) -> bool {
  matches!(
    action,
    Action::ScrollUp
      | Action::ScrollDown
      | Action::ScrollLeft
      | Action::ScrollRight
      | Action::ScrollForward
      | Action::ScrollBackward
      | Action::ScrollToPoint
      | Action::SetScrollOffset
  )
}

/// Build a minimal AccessKit tree update describing the document viewport and any element scroll
/// containers.
///
/// The returned tree intentionally contains *only* scroll container nodes:
/// - root node: viewport scroll container
/// - children: element scroll containers (overflow: auto|scroll|hidden)
///
/// Higher-level accessibility tree building (roles/names/relationships) is handled elsewhere.
pub fn build_scroll_container_tree_update(
  fragment_tree: &FragmentTree,
  viewport: Size,
  scroll_state: &ScrollState,
) -> TreeUpdate {
  let mut classes = NodeClassSet::new();
  let mut nodes: Vec<(NodeId, Node)> = Vec::new();

  // Collect scroll containers keyed by box id, along with their absolute bounds in page coordinates.
  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a FragmentNode,
    abs_origin: Point,
    has_fixed_cb_ancestor: bool,
  }

  let mut abs_bounds_by_box_id: HashMap<usize, crate::geometry::Rect> = HashMap::new();
  let mut element_scroll_containers: HashMap<usize, ScrollContainerInfo> = HashMap::new();

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      abs_origin: Point::ZERO,
      has_fixed_cb_ancestor: false,
    });
  }
  stack.push(Frame {
    node: &fragment_tree.root,
    abs_origin: Point::ZERO,
    has_fixed_cb_ancestor: false,
  });

  while let Some(frame) = stack.pop() {
    let node = frame.node;
    let abs_origin = frame
      .abs_origin
      .translate(Point::new(node.bounds.x(), node.bounds.y()));
    let abs_rect = node.bounds.translate(frame.abs_origin);

    if let Some(box_id) = node.box_id() {
      abs_bounds_by_box_id
        .entry(box_id)
        .and_modify(|existing| *existing = existing.union(abs_rect))
        .or_insert(abs_rect);

      if fragment_is_scroll_container(node) && !element_scroll_containers.contains_key(&box_id) {
        let viewport_local = node.bounds.size;
        let bounds = crate::scroll::scroll_bounds_for_fragment(
          node,
          Point::ZERO,
          viewport_local,
          viewport,
          false,
          frame.has_fixed_cb_ancestor,
        );
        let scrollport = node
          .style
          .as_deref()
          .map(|style| crate::scroll::scrollport_rect_for_fragment(node, style).size)
          .unwrap_or(viewport_local);
        element_scroll_containers.insert(box_id, ScrollContainerInfo { bounds, scrollport });
      }
    }

    let establishes_fixed_cb = node
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_fixed_containing_block());
    let child_fixed = frame.has_fixed_cb_ancestor || establishes_fixed_cb;

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        abs_origin,
        has_fixed_cb_ancestor: child_fixed,
      });
    }
  }

  // Build element scroll container nodes.
  let mut child_ids: Vec<NodeId> = Vec::new();
  let viewport_scroll = sanitize_point_nonneg(scroll_state.viewport);

  let mut box_ids: Vec<usize> = element_scroll_containers.keys().copied().collect();
  box_ids.sort_unstable();

  for box_id in box_ids {
    let info = element_scroll_containers
      .get(&box_id)
      .copied()
      .expect("box_id came from keys"); // fastrender-allow-unwrap
    let node_id = node_id_for_scroll_box_id(box_id);
    child_ids.push(node_id);

    let mut builder = NodeBuilder::new(accesskit::Role::GenericContainer);
    let abs_bounds = abs_bounds_by_box_id
      .get(&box_id)
      .copied()
      .unwrap_or_else(|| {
        // Fallback for unexpected cases where we collected a scroll container without bounds.
        crate::geometry::Rect::from_xywh(0.0, 0.0, 0.0, 0.0)
      });
    let viewport_bounds = abs_bounds.translate(Point::new(-viewport_scroll.x, -viewport_scroll.y));
    builder.set_bounds(accesskit_rect_from_xywh(
      viewport_bounds.x(),
      viewport_bounds.y(),
      viewport_bounds.width(),
      viewport_bounds.height(),
    ));

    let current = scroll_state.element_offset(box_id);
    set_scroll_properties(&mut builder, current, info.bounds, info.scrollport);

    nodes.push((node_id, builder.build(&mut classes)));
  }

  // Root scroll container node (viewport scroll).
  {
    let root_info = scroll_container_info_for_root(fragment_tree, viewport);
    let mut builder = NodeBuilder::new(accesskit::Role::Document);
    builder.set_bounds(accesskit_rect_from_xywh(
      0.0,
      0.0,
      viewport.width,
      viewport.height,
    ));
    builder.set_children(child_ids.clone());
    set_scroll_properties(
      &mut builder,
      scroll_state.viewport,
      root_info.bounds,
      root_info.scrollport,
    );
    nodes.push((ROOT_SCROLL_CONTAINER_ID, builder.build(&mut classes)));
  }

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(ROOT_SCROLL_CONTAINER_ID)),
    focus: None,
  }
}

fn scroll_target_from_request(request: &ActionRequest) -> Option<ScrollTarget> {
  if request.target == ROOT_SCROLL_CONTAINER_ID {
    return Some(ScrollTarget::Viewport);
  }
  scroll_box_id_for_node_id(request.target).map(ScrollTarget::Element)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
  Viewport,
  Element(usize),
}

/// Apply a scroll-related AccessKit action to the given scroll state, returning the updated state.
///
/// Returns `None` when the request is not a supported scroll action, or when it results in no scroll
/// offset change after clamping/sanitization.
pub fn apply_scroll_action_to_scroll_state(
  fragment_tree: &FragmentTree,
  viewport: Size,
  current: &ScrollState,
  request: &ActionRequest,
) -> Option<ScrollState> {
  if !is_scroll_action(request.action) {
    return None;
  }

  let target = scroll_target_from_request(request)?;
  let root_viewport = viewport;

  let (info, current_offset) = match target {
    ScrollTarget::Viewport => (
      scroll_container_info_for_root(fragment_tree, viewport),
      sanitize_point_nonneg(current.viewport),
    ),
    ScrollTarget::Element(box_id) => {
      let info = scroll_container_info_for_box_id(fragment_tree, root_viewport, box_id)?;
      (info, sanitize_point_nonneg(current.element_offset(box_id)))
    }
  };

  let line_step = 40.0_f32;
  let page_step_x = if info.scrollport.width.is_finite() {
    info.scrollport.width.max(0.0)
  } else {
    0.0
  };
  let page_step_y = if info.scrollport.height.is_finite() {
    info.scrollport.height.max(0.0)
  } else {
    0.0
  };

  let mut next_offset = current_offset;

  match request.action {
    Action::ScrollUp => {
      next_offset.y -= line_step;
    }
    Action::ScrollDown => {
      next_offset.y += line_step;
    }
    Action::ScrollLeft => {
      next_offset.x -= line_step;
    }
    Action::ScrollRight => {
      next_offset.x += line_step;
    }
    Action::ScrollForward => {
      if info.bounds.max_y > info.bounds.min_y {
        next_offset.y += page_step_y;
      } else {
        next_offset.x += page_step_x;
      }
    }
    Action::ScrollBackward => {
      if info.bounds.max_y > info.bounds.min_y {
        next_offset.y -= page_step_y;
      } else {
        next_offset.x -= page_step_x;
      }
    }
    Action::ScrollToPoint => {
      let data = request.data.as_ref()?;
      let accesskit::ActionData::ScrollToPoint(p) = data else {
        return None;
      };
      next_offset = Point::new(p.x as f32, p.y as f32);
    }
    Action::SetScrollOffset => {
      let data = request.data.as_ref()?;
      let accesskit::ActionData::SetScrollOffset(p) = data else {
        return None;
      };
      next_offset = Point::new(p.x as f32, p.y as f32);
    }
    _ => return None,
  }

  next_offset = sanitize_point_nonneg(next_offset);
  next_offset = clamp_point_to_bounds(next_offset, info.bounds);

  if next_offset == current_offset {
    return None;
  }

  let delta = Point::new(
    next_offset.x - current_offset.x,
    next_offset.y - current_offset.y,
  );

  let mut next = ScrollState::from_parts(current.viewport, current.elements.clone());

  match target {
    ScrollTarget::Viewport => {
      next.viewport = next_offset;
      next.viewport_delta = delta;
      // `elements_delta` remains empty (no element scroll occurred).
    }
    ScrollTarget::Element(box_id) => {
      next.viewport_delta = Point::ZERO;
      next.elements.insert(box_id, next_offset);
      if delta != Point::ZERO {
        next.elements_delta.insert(box_id, delta);
      }
    }
  }

  Some(next)
}
