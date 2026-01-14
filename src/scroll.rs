use std::collections::HashMap;

use crate::geometry::{Point, Rect, Size};
use crate::paint::display_list::Transform3D;
use crate::style::position::Position;
use crate::style::types::{
  BackgroundAttachment, Direction, Overflow, OverscrollBehavior, ScrollBehavior, ScrollSnapAlign,
  ScrollSnapAxis, ScrollSnapStop, ScrollSnapStrictness, VisualBox, WritingMode,
};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::style::{
  block_axis_is_horizontal, block_axis_positive, inline_axis_is_horizontal, inline_axis_positive,
  PhysicalSide,
};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
pub mod anchoring;
pub use anchoring::{
  apply_scroll_anchoring, apply_scroll_anchoring_between_trees, capture_scroll_anchors, ScrollAnchor,
  ScrollAnchorSnapshot,
};
pub(crate) use anchoring::apply_scroll_anchoring_with_scroll_snap;
pub(crate) mod anchoring_debug;

/// Viewport and element scroll offsets used when applying scroll snap.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollState {
  /// Document/viewport scroll offset
  pub viewport: Point,
  /// Scroll offsets for element scroll containers keyed by box_id
  pub elements: HashMap<usize, Point>,
  /// Most recent viewport scroll delta (relative offset change).
  pub viewport_delta: Point,
  /// Most recent scroll deltas for element scroll containers keyed by box_id.
  pub elements_delta: HashMap<usize, Point>,
}

impl ScrollState {
  /// Creates a scroll state with only a viewport offset.
  pub fn with_viewport(viewport: Point) -> Self {
    Self::from_parts(viewport, HashMap::new())
  }

  /// Creates a scroll state with explicit viewport and element offsets.
  pub fn from_parts(viewport: Point, elements: HashMap<usize, Point>) -> Self {
    Self {
      viewport,
      elements,
      viewport_delta: Point::ZERO,
      elements_delta: HashMap::new(),
    }
  }

  /// Creates a scroll state with explicit offsets and deltas.
  pub fn from_parts_with_deltas(
    viewport: Point,
    elements: HashMap<usize, Point>,
    viewport_delta: Point,
    elements_delta: HashMap<usize, Point>,
  ) -> Self {
    Self {
      viewport,
      elements,
      viewport_delta,
      elements_delta,
    }
  }

  /// Returns the stored scroll offset for an element, if present.
  pub fn element_offset(&self, id: usize) -> Point {
    self.elements.get(&id).copied().unwrap_or(Point::ZERO)
  }

  /// Returns the stored scroll delta for an element, if present.
  pub fn element_delta(&self, id: usize) -> Point {
    self.elements_delta.get(&id).copied().unwrap_or(Point::ZERO)
  }

  /// Recomputes scroll deltas relative to `prev`, overwriting `viewport_delta` and `elements_delta`.
  ///
  /// This is intended for callers that update scroll offsets programmatically (focus scroll,
  /// fragment navigation, scroll-to actions, etc). It prevents stale deltas from leaking across
  /// independent scroll operations by deriving new deltas from the offset changes between `prev`
  /// and `self`.
  ///
  /// Behaviour:
  /// - `viewport_delta = self.viewport - prev.viewport`
  /// - `elements_delta[id] = self.elements[id] - prev.elements.get(id).unwrap_or(Point::ZERO)`
  ///   for each scroll container whose effective offset changed (missing entries are treated as
  ///   `Point::ZERO`).
  /// - Non-finite delta components are treated as `0.0`.
  /// - `elements` and `elements_delta` omit `Point::ZERO` entries for canonical representation.
  pub fn update_deltas_from(&mut self, prev: &ScrollState) {
    let sanitize = |point: Point| {
      Point::new(
        if point.x.is_finite() { point.x } else { 0.0 },
        if point.y.is_finite() { point.y } else { 0.0 },
      )
    };

    let prev_viewport = sanitize(prev.viewport);
    let next_viewport = sanitize(self.viewport);
    self.viewport_delta = sanitize(Point::new(
      next_viewport.x - prev_viewport.x,
      next_viewport.y - prev_viewport.y,
    ));

    let mut elements_delta: HashMap<usize, Point> = HashMap::new();

    // Track deltas for any element scroll container whose effective offset changed. Note that the
    // canonical representation omits `Point::ZERO` offsets entirely, so we must treat "missing"
    // and "zero" as equivalent when diffing.
    for (&id, &prev_offset_raw) in prev.elements.iter() {
      let prev_offset = sanitize(prev_offset_raw);
      let next_offset = sanitize(self.elements.get(&id).copied().unwrap_or(Point::ZERO));
      if next_offset != prev_offset {
        let delta = sanitize(Point::new(
          next_offset.x - prev_offset.x,
          next_offset.y - prev_offset.y,
        ));
        if delta != Point::ZERO {
          elements_delta.insert(id, delta);
        }
      }
    }
    for (&id, &next_offset_raw) in self.elements.iter() {
      if prev.elements.contains_key(&id) {
        continue;
      }
      let next_offset = sanitize(next_offset_raw);
      if next_offset != Point::ZERO {
        let delta = sanitize(next_offset);
        if delta != Point::ZERO {
          elements_delta.insert(id, delta);
        }
      }
    }

    // Preserve canonical representation so missing and zero offsets/deltas are treated the same.
    self.elements.retain(|_, offset| *offset != Point::ZERO);
    elements_delta.retain(|_, delta| *delta != Point::ZERO);
    self.elements_delta = elements_delta;
  }
}

impl Default for ScrollState {
  fn default() -> Self {
    Self::with_viewport(Point::ZERO)
  }
}

/// A single snap target along an axis.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollSnapTarget {
  pub box_id: Option<usize>,
  pub position: f32,
  pub stop: ScrollSnapStop,
}

/// Metadata for a scroll snap container.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollSnapContainer {
  pub box_id: Option<usize>,
  pub viewport: Size,
  pub strictness: ScrollSnapStrictness,
  pub behavior: ScrollBehavior,
  pub snap_x: bool,
  pub snap_y: bool,
  pub axis_is_inline_for_x: bool,
  pub axis_is_inline_for_y: bool,
  pub padding_x: (f32, f32),
  pub padding_y: (f32, f32),
  pub scroll_bounds: Rect,
  pub targets_x: Vec<ScrollSnapTarget>,
  pub targets_y: Vec<ScrollSnapTarget>,
  pub uses_viewport_scroll: bool,
}

/// Aggregated scroll snap metadata for a fragment tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScrollMetadata {
  pub containers: Vec<ScrollSnapContainer>,
}

/// Result of applying scroll snap to a scroll state.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollSnapUpdate {
  pub container: Option<usize>,
  pub offset: Point,
  pub behavior: ScrollBehavior,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScrollSnapResult {
  pub state: ScrollState,
  pub updates: Vec<ScrollSnapUpdate>,
}

fn is_vertical_writing_mode(mode: WritingMode) -> bool {
  matches!(
    mode,
    WritingMode::VerticalRl
      | WritingMode::VerticalLr
      | WritingMode::SidewaysRl
      | WritingMode::SidewaysLr
  )
}

fn axis_sides(
  mode: WritingMode,
  direction: Direction,
  inline_axis: bool,
) -> (PhysicalSide, PhysicalSide) {
  let horizontal = if inline_axis {
    inline_axis_is_horizontal(mode)
  } else {
    block_axis_is_horizontal(mode)
  };
  let positive = if inline_axis {
    inline_axis_positive(mode, direction)
  } else {
    block_axis_positive(mode)
  };

  if horizontal {
    if positive {
      (PhysicalSide::Left, PhysicalSide::Right)
    } else {
      (PhysicalSide::Right, PhysicalSide::Left)
    }
  } else if positive {
    (PhysicalSide::Top, PhysicalSide::Bottom)
  } else {
    (PhysicalSide::Bottom, PhysicalSide::Top)
  }
}

fn base_for_side(side: PhysicalSide, viewport: Size) -> f32 {
  match side {
    PhysicalSide::Left | PhysicalSide::Right => viewport.width,
    PhysicalSide::Top | PhysicalSide::Bottom => viewport.height,
  }
}

fn resolve_snap_length(len: Length, percentage_base: f32) -> f32 {
  len
    .resolve_against(percentage_base)
    .unwrap_or_else(|| len.to_px())
}

fn sanitize_snap_length(value: f32) -> f32 {
  if value.is_finite() {
    value
  } else {
    0.0
  }
}

fn sanitize_scroll_padding(value: f32) -> f32 {
  sanitize_snap_length(value).max(0.0)
}

fn scroll_padding_for_side(style: &ComputedStyle, side: PhysicalSide) -> Length {
  match side {
    PhysicalSide::Top => style.scroll_padding_top,
    PhysicalSide::Right => style.scroll_padding_right,
    PhysicalSide::Bottom => style.scroll_padding_bottom,
    PhysicalSide::Left => style.scroll_padding_left,
  }
}

fn scroll_margin_for_side(style: &ComputedStyle, side: PhysicalSide) -> Length {
  match side {
    PhysicalSide::Top => style.scroll_margin_top,
    PhysicalSide::Right => style.scroll_margin_right,
    PhysicalSide::Bottom => style.scroll_margin_bottom,
    PhysicalSide::Left => style.scroll_margin_left,
  }
}

fn snap_axis_flags(axis: ScrollSnapAxis, inline_vertical: bool) -> (bool, bool) {
  match axis {
    ScrollSnapAxis::None => (false, false),
    ScrollSnapAxis::Both => (true, true),
    ScrollSnapAxis::X => (true, false),
    ScrollSnapAxis::Y => (false, true),
    ScrollSnapAxis::Inline => {
      if inline_vertical {
        (false, true)
      } else {
        (true, false)
      }
    }
    ScrollSnapAxis::Block => {
      if inline_vertical {
        (true, false)
      } else {
        (false, true)
      }
    }
  }
}

fn snap_position(
  alignment: ScrollSnapAlign,
  phys_start: f32,
  phys_end: f32,
  viewport_extent: f32,
  padding_start: f32,
  padding_end: f32,
  margin_start: f32,
  margin_end: f32,
  axis_positive: bool,
) -> Option<f32> {
  let target_start = if axis_positive {
    phys_start - margin_start
  } else {
    phys_end + margin_start
  };
  let target_end = if axis_positive {
    phys_end + margin_end
  } else {
    phys_start - margin_end
  };

  let snapport_start_offset = if axis_positive {
    padding_start
  } else {
    viewport_extent - padding_start
  };
  let snapport_end_offset = if axis_positive {
    viewport_extent - padding_end
  } else {
    padding_end
  };

  let pos = match alignment {
    ScrollSnapAlign::None => None,
    ScrollSnapAlign::Start => Some(target_start - snapport_start_offset),
    ScrollSnapAlign::End => Some(target_end - snapport_end_offset),
    ScrollSnapAlign::Center => {
      let target_center = (target_start + target_end) * 0.5;
      let snapport_center = (snapport_start_offset + snapport_end_offset) * 0.5;
      Some(target_center - snapport_center)
    }
  };

  pos.and_then(|pos| pos.is_finite().then_some(pos))
}

fn pick_snap_target(
  current: f32,
  max_scroll: f32,
  strictness: ScrollSnapStrictness,
  threshold: f32,
  candidates: &[(f32, ScrollSnapStop)],
) -> f32 {
  // Scroll snapping must not panic, even when upstream layout produces NaN/+inf/-inf geometry.
  // Any non-finite candidate positions are ignored and, if none remain, we fall back to clamping
  // the current offset.
  if !current.is_finite() || !max_scroll.is_finite() {
    return current;
  }

  let max_scroll = max_scroll.max(0.0);
  if candidates.is_empty() {
    return current.clamp(0.0, max_scroll);
  }

  let mut best: Option<(f32, f32, bool)> = None;
  let epsilon = 1e-3;

  for &(candidate, stop) in candidates {
    if !candidate.is_finite() {
      continue;
    }

    let clamped = candidate.clamp(0.0, max_scroll);
    let dist = (clamped - current).abs();
    if !dist.is_finite() {
      continue;
    }
    let stop_always = stop == ScrollSnapStop::Always;

    let replace = match best {
      None => true,
      Some((best_pos, best_dist, best_stop_always)) => {
        if dist + epsilon < best_dist {
          true
        } else if (dist - best_dist).abs() <= epsilon {
          if stop_always && !best_stop_always {
            true
          } else if stop_always == best_stop_always {
            clamped.total_cmp(&best_pos).is_lt()
          } else {
            false
          }
        } else {
          false
        }
      }
    };

    if replace {
      best = Some((clamped, dist, stop_always));
    }
  }

  let Some((best_pos, best_dist, _)) = best else {
    return current.clamp(0.0, max_scroll);
  };

  match strictness {
    ScrollSnapStrictness::Mandatory => best_pos,
    ScrollSnapStrictness::Proximity => {
      if best_dist <= threshold {
        best_pos
      } else {
        current
      }
    }
  }
}

fn fragment_box_id(fragment: &FragmentNode) -> Option<usize> {
  match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Text { box_id, .. }
    | FragmentContent::Replaced { box_id, .. } => *box_id,
    FragmentContent::RunningAnchor { .. } => None,
    FragmentContent::FootnoteAnchor { .. } => None,
    FragmentContent::Line { .. } => None,
  }
}

#[inline]
fn overflow_axis_clips(overflow: Overflow) -> bool {
  matches!(
    overflow,
    Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
  )
}

fn resolve_length_with_context(
  length: Length,
  percentage_base: Option<f32>,
  viewport: Size,
  style: &ComputedStyle,
) -> f32 {
  length
    .resolve_with_context(
      percentage_base,
      viewport.width,
      viewport.height,
      style.font_size,
      style.root_font_size,
    )
    .unwrap_or_else(|| length.to_px())
}

fn sanitize_nonneg(value: f32) -> f32 {
  if value.is_finite() {
    value.max(0.0)
  } else {
    0.0
  }
}

fn used_border_insets(style: &ComputedStyle) -> (f32, f32, f32, f32) {
  (
    sanitize_nonneg(style.used_border_left_width().to_px()),
    sanitize_nonneg(style.used_border_right_width().to_px()),
    sanitize_nonneg(style.used_border_top_width().to_px()),
    sanitize_nonneg(style.used_border_bottom_width().to_px()),
  )
}

/// Returns the scrollport rectangle for `node` in the fragment's local coordinate space.
///
/// The scrollport is the viewport through which a scroll container's contents are scrolled.
/// `FragmentNode::bounds` describes the fragment's border box; the scrollport corresponds to the
/// padding box, further reduced by any reserved classic scrollbar gutters.
pub fn scrollport_rect_for_fragment(node: &FragmentNode, style: &ComputedStyle) -> Rect {
  let width = node.bounds.width();
  let height = node.bounds.height();
  let (border_left, border_right, border_top, border_bottom) = used_border_insets(style);

  let mut rect = Rect::from_xywh(
    border_left,
    border_top,
    (width - border_left - border_right).max(0.0),
    (height - border_top - border_bottom).max(0.0),
  );

  let reservation = node.scrollbar_reservation;
  let reserve_left = sanitize_nonneg(reservation.left);
  let reserve_right = sanitize_nonneg(reservation.right);
  let reserve_top = sanitize_nonneg(reservation.top);
  let reserve_bottom = sanitize_nonneg(reservation.bottom);

  rect.origin.x += reserve_left;
  rect.origin.y += reserve_top;
  rect.size.width = (rect.size.width - reserve_left - reserve_right).max(0.0);
  rect.size.height = (rect.size.height - reserve_top - reserve_bottom).max(0.0);

  rect
}

pub fn client_width_height_for_fragment(node: &FragmentNode, style: &ComputedStyle) -> (f32, f32) {
  let rect = scrollport_rect_for_fragment(node, style);
  (rect.size.width, rect.size.height)
}

// === Scroll anchoring (CSS Scroll Anchoring Module Level 1) ===
//
// The primary scroll anchoring implementation is tracked elsewhere, but unit tests depend on
// having a deterministic anchor *selection* routine. Keep this helper internal and lint-clean for
// non-test builds (CI runs `cargo doc -D warnings`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn select_scroll_anchoring_anchor_box_id(
  scroller: &FragmentNode,
  scroll_offset: Point,
) -> Option<usize> {
  let style = scroller.style.as_deref()?;
  if style.overflow_anchor == crate::style::types::OverflowAnchor::None {
    return None;
  }

  let scrollport = scrollport_rect_for_fragment(scroller, style);

  let pad_left = sanitize_scroll_padding(resolve_snap_length(
    style.scroll_padding_left,
    scrollport.size.width,
  ));
  let pad_right = sanitize_scroll_padding(resolve_snap_length(
    style.scroll_padding_right,
    scrollport.size.width,
  ));
  let pad_top = sanitize_scroll_padding(resolve_snap_length(
    style.scroll_padding_top,
    scrollport.size.height,
  ));
  let pad_bottom = sanitize_scroll_padding(resolve_snap_length(
    style.scroll_padding_bottom,
    scrollport.size.height,
  ));

  let optimal_viewing_region = Rect::from_xywh(
    scrollport.origin.x + pad_left,
    scrollport.origin.y + pad_top,
    (scrollport.size.width - pad_left - pad_right).max(0.0),
    (scrollport.size.height - pad_top - pad_bottom).max(0.0),
  );

  fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.min_x() >= outer.min_x()
      && inner.max_x() <= outer.max_x()
      && inner.min_y() >= outer.min_y()
      && inner.max_y() <= outer.max_y()
  }

  fn candidate_examination(
    node: &FragmentNode,
    origin: Point,
    scroll_offset: Point,
    optimal_viewing_region: Rect,
  ) -> Option<usize> {
    if node
      .style
      .as_deref()
      .is_some_and(|style| style.overflow_anchor == crate::style::types::OverflowAnchor::None)
    {
      return None;
    }

    let bounding_rect = node.scroll_overflow.translate(Point::new(
      origin.x - scroll_offset.x,
      origin.y - scroll_offset.y,
    ));

    if !bounding_rect.intersects(optimal_viewing_region) {
      // Fully clipped: the scroll anchoring spec says we must skip this node and its descendants.
      return None;
    }

    let box_id = node.box_id();
    if box_id.is_some() && contains_rect(optimal_viewing_region, bounding_rect) {
      return box_id;
    }

    // Partially visible. Search descendants first.
    let child_base = origin;
    for child in node.children.iter() {
      let child_origin = Point::new(
        child_base.x + child.bounds.x(),
        child_base.y + child.bounds.y(),
      );
      if let Some(anchor) =
        candidate_examination(child, child_origin, scroll_offset, optimal_viewing_region)
      {
        return Some(anchor);
      }
    }

    box_id
  }

  for child in scroller.children.iter() {
    let child_origin = Point::new(child.bounds.x(), child.bounds.y());
    if let Some(anchor) =
      candidate_examination(child, child_origin, scroll_offset, optimal_viewing_region)
    {
      return Some(anchor);
    }
  }

  None
}

fn content_rect(node: &FragmentNode, style: &ComputedStyle, viewport: Size) -> Rect {
  let mut rect = scrollport_rect_for_fragment(node, style);
  let percentage_base = node.bounds.width().max(0.0);
  let left = sanitize_nonneg(resolve_length_with_context(
    style.padding_left,
    Some(percentage_base),
    viewport,
    style,
  ));
  let right = sanitize_nonneg(resolve_length_with_context(
    style.padding_right,
    Some(percentage_base),
    viewport,
    style,
  ));
  let top = sanitize_nonneg(resolve_length_with_context(
    style.padding_top,
    Some(percentage_base),
    viewport,
    style,
  ));
  let bottom = sanitize_nonneg(resolve_length_with_context(
    style.padding_bottom,
    Some(percentage_base),
    viewport,
    style,
  ));

  rect.origin.x += left;
  rect.origin.y += top;
  rect.size.width = (rect.size.width - left - right).max(0.0);
  rect.size.height = (rect.size.height - top - bottom).max(0.0);
  rect
}

fn overflow_clip_rect(node: &FragmentNode, style: &ComputedStyle, viewport: Size) -> Rect {
  let border = Rect::from_xywh(0.0, 0.0, node.bounds.width(), node.bounds.height());
  let padding = scrollport_rect_for_fragment(node, style);
  let content = content_rect(node, style, viewport);

  let resolved_margin = resolve_length_with_context(
    style.overflow_clip_margin.margin,
    Some(border.width().max(0.0)),
    viewport,
    style,
  );
  let margin = sanitize_nonneg(resolved_margin);

  let rect_for_visual_box = |vb: VisualBox| match vb {
    VisualBox::BorderBox => border,
    VisualBox::PaddingBox => padding,
    VisualBox::ContentBox => content,
  };

  let base_x = if matches!(style.overflow_x, Overflow::Clip) {
    rect_for_visual_box(style.overflow_clip_margin.visual_box)
  } else {
    padding
  };
  let base_y = if matches!(style.overflow_y, Overflow::Clip) {
    rect_for_visual_box(style.overflow_clip_margin.visual_box)
  } else {
    padding
  };

  let expand_x = if matches!(style.overflow_x, Overflow::Clip) {
    margin
  } else {
    0.0
  };
  let expand_y = if matches!(style.overflow_y, Overflow::Clip) {
    margin
  } else {
    0.0
  };

  let min_x = base_x.min_x() - expand_x;
  let max_x = base_x.max_x() + expand_x;
  let min_y = base_y.min_y() - expand_y;
  let max_y = base_y.max_y() + expand_y;

  Rect::from_xywh(
    min_x,
    min_y,
    (max_x - min_x).max(0.0),
    (max_y - min_y).max(0.0),
  )
}

fn clip_rect_from_style(
  node: &FragmentNode,
  style: &ComputedStyle,
  viewport: Size,
) -> Option<Rect> {
  if !matches!(style.position, Position::Absolute | Position::Fixed) {
    return None;
  }
  let clip = style.clip.as_ref()?;
  let width = node.bounds.width().max(0.0);
  let height = node.bounds.height().max(0.0);

  let resolve_component =
    |component: &crate::style::types::ClipComponent, base: f32| match component {
      crate::style::types::ClipComponent::Auto => None,
      crate::style::types::ClipComponent::Length(len) => Some(resolve_length_with_context(
        *len,
        Some(base),
        viewport,
        style,
      )),
    };

  let left_offset = resolve_component(&clip.left, width).unwrap_or(0.0);
  let top_offset = resolve_component(&clip.top, height).unwrap_or(0.0);
  let right_offset = resolve_component(&clip.right, width).unwrap_or(width);
  let bottom_offset = resolve_component(&clip.bottom, height).unwrap_or(height);

  Some(Rect::from_xywh(
    left_offset,
    top_offset,
    (right_offset - left_offset).max(0.0),
    (bottom_offset - top_offset).max(0.0),
  ))
}

fn annotate_overflow(node: &mut FragmentNode, has_fixed_cb_ancestor: bool, viewport: Size) -> Rect {
  let mut overflow = Rect::from_xywh(0.0, 0.0, node.bounds.width(), node.bounds.height());

  // `FragmentContent::RunningAnchor` / `FootnoteAnchor` store an out-of-flow snapshot subtree
  // outside of the normal `children` list. These snapshots do not participate in ancestor
  // overflow propagation, but downstream code (display-list culling, scroll bounds for the snapshot
  // itself, etc.) still expects their `scroll_overflow` fields to be populated.
  //
  // Compute their overflow eagerly here without unioning it into `overflow`.
  match &mut node.content {
    FragmentContent::RunningAnchor { snapshot, .. }
    | FragmentContent::FootnoteAnchor { snapshot, .. } => {
      annotate_overflow(
        std::sync::Arc::make_mut(snapshot),
        has_fixed_cb_ancestor,
        viewport,
      );
    }
    _ => {}
  }

  let has_fixed_cb_ancestor = has_fixed_cb_ancestor
    || node
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_fixed_containing_block());
  for child in node.children_mut() {
    let child_overflow = annotate_overflow(child, has_fixed_cb_ancestor, viewport);
    let translated = child_overflow.translate(Point::new(child.bounds.x(), child.bounds.y()));
    let child_is_viewport_fixed = child
      .style
      .as_deref()
      .is_some_and(|style| matches!(style.position, Position::Fixed))
      && !has_fixed_cb_ancestor;
    if child_is_viewport_fixed {
      continue;
    }

    let child_style = child.style.as_ref();
    let clips_x = child_style
      .map(|style| overflow_axis_clips(style.overflow_x))
      .unwrap_or(false);
    let clips_y = child_style
      .map(|style| overflow_axis_clips(style.overflow_y))
      .unwrap_or(false);

    let mut clip = AxisClipRect::unbounded();
    if clips_x || clips_y {
      if let Some(style) = child_style {
        let local_clip = overflow_clip_rect(child, style, viewport);
        let clip_rect = local_clip.translate(Point::new(child.bounds.x(), child.bounds.y()));
        if let Some(updated) = clip.clip_to_rect_on_axes(clip_rect, clips_x, clips_y) {
          clip = updated;
        }
      }
    }

    if let Some(style) = child_style {
      if let Some(local_clip) = clip_rect_from_style(child, style, viewport) {
        let clip_rect = local_clip.translate(Point::new(child.bounds.x(), child.bounds.y()));
        if let Some(updated) = clip.clip_to_rect_on_axes(clip_rect, true, true) {
          clip = updated;
        }
      }
    }

    if let Some(mut clipped) = clip.intersect_rect(translated) {
      // CSS transforms affect the visual overflow of descendants. The scrollable overflow region is
      // defined in terms of *painted* boxes, so account for transforms when propagating child
      // overflow into ancestors.
      if let Some(style) = child_style {
        if style.has_transform() || style.has_motion_path() {
          let bounds = Rect::from_xywh(
            child.bounds.x(),
            child.bounds.y(),
            child.bounds.width(),
            child.bounds.height(),
          );
          if let Some(transform) = crate::paint::transform_resolver::resolve_transform3d(
            style,
            bounds,
            Some((viewport.width, viewport.height)),
          ) {
            clipped = transform.transform_rect(clipped);
          }
        }
      }

      overflow = overflow.union(clipped);
    }
  }
  node.scroll_overflow = overflow;
  overflow
}

struct PendingContainer {
  origin: Point,
  viewport: Size,
  strictness: ScrollSnapStrictness,
  behavior: ScrollBehavior,
  snap_x: bool,
  snap_y: bool,
  inline_sides: (PhysicalSide, PhysicalSide),
  block_sides: (PhysicalSide, PhysicalSide),
  axis_is_inline_for_x: bool,
  axis_is_inline_for_y: bool,
  start_positive_x: bool,
  start_positive_y: bool,
  padding_x: (f32, f32),
  padding_y: (f32, f32),
  content_bounds: Rect,
  targets_x: Vec<ScrollSnapTarget>,
  targets_y: Vec<ScrollSnapTarget>,
  box_id: Option<usize>,
  uses_viewport_scroll: bool,
}

impl PendingContainer {
  fn new(
    node: &FragmentNode,
    style: &ComputedStyle,
    origin: Point,
    uses_viewport_scroll: bool,
    viewport: Size,
  ) -> Option<Self> {
    let inline_vertical = is_vertical_writing_mode(style.writing_mode);
    let (snap_x, snap_y) = snap_axis_flags(style.scroll_snap_type.axis, inline_vertical);
    if !snap_x && !snap_y {
      return None;
    }

    let axis_is_inline_for_x = !inline_vertical;
    let axis_is_inline_for_y = inline_vertical;
    let inline_sides = axis_sides(style.writing_mode, style.direction, true);
    let block_sides = axis_sides(style.writing_mode, style.direction, false);
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    let block_positive = block_axis_positive(style.writing_mode);
    let padding_x_sides = if axis_is_inline_for_x {
      inline_sides
    } else {
      block_sides
    };
    let padding_y_sides = if axis_is_inline_for_y {
      inline_sides
    } else {
      block_sides
    };
    let padding_x = (
      sanitize_scroll_padding(resolve_snap_length(
        scroll_padding_for_side(style, padding_x_sides.0),
        base_for_side(padding_x_sides.0, viewport),
      )),
      sanitize_scroll_padding(resolve_snap_length(
        scroll_padding_for_side(style, padding_x_sides.1),
        base_for_side(padding_x_sides.1, viewport),
      )),
    );
    let padding_y = (
      sanitize_scroll_padding(resolve_snap_length(
        scroll_padding_for_side(style, padding_y_sides.0),
        base_for_side(padding_y_sides.0, viewport),
      )),
      sanitize_scroll_padding(resolve_snap_length(
        scroll_padding_for_side(style, padding_y_sides.1),
        base_for_side(padding_y_sides.1, viewport),
      )),
    );

    Some(Self {
      origin,
      viewport,
      strictness: style.scroll_snap_type.strictness,
      behavior: style.scroll_behavior,
      snap_x,
      snap_y,
      inline_sides,
      block_sides,
      axis_is_inline_for_x,
      axis_is_inline_for_y,
      start_positive_x: if axis_is_inline_for_x {
        inline_positive
      } else {
        block_positive
      },
      start_positive_y: if axis_is_inline_for_y {
        inline_positive
      } else {
        block_positive
      },
      padding_x,
      padding_y,
      content_bounds: node.scroll_overflow,
      targets_x: Vec::new(),
      targets_y: Vec::new(),
      box_id: fragment_box_id(node),
      uses_viewport_scroll,
    })
  }

  fn collect_targets(&mut self, style: &ComputedStyle, node: &FragmentNode, origin: Point) {
    let rel_bounds = Rect::from_xywh(
      origin.x - self.origin.x,
      origin.y - self.origin.y,
      node.bounds.width(),
      node.bounds.height(),
    );

    if self.snap_x {
      let sides = if self.axis_is_inline_for_x {
        self.inline_sides
      } else {
        self.block_sides
      };
      let base = base_for_side(sides.0, self.viewport);
      let margin_start = sanitize_snap_length(resolve_snap_length(
        scroll_margin_for_side(style, sides.0),
        base,
      ));
      let margin_end = sanitize_snap_length(resolve_snap_length(
        scroll_margin_for_side(style, sides.1),
        base,
      ));
      let align = if self.axis_is_inline_for_x {
        style.scroll_snap_align.inline
      } else {
        style.scroll_snap_align.block
      };
      if let Some(pos) = snap_position(
        align,
        rel_bounds.min_x(),
        rel_bounds.max_x(),
        self.viewport.width,
        self.padding_x.0,
        self.padding_x.1,
        margin_start,
        margin_end,
        self.start_positive_x,
      ) {
        self.targets_x.push(ScrollSnapTarget {
          box_id: fragment_box_id(node),
          position: pos,
          stop: style.scroll_snap_stop,
        });
      }
    }

    if self.snap_y {
      let sides = if self.axis_is_inline_for_y {
        self.inline_sides
      } else {
        self.block_sides
      };
      let base = base_for_side(sides.0, self.viewport);
      let margin_start = sanitize_snap_length(resolve_snap_length(
        scroll_margin_for_side(style, sides.0),
        base,
      ));
      let margin_end = sanitize_snap_length(resolve_snap_length(
        scroll_margin_for_side(style, sides.1),
        base,
      ));
      let align = if self.axis_is_inline_for_y {
        style.scroll_snap_align.inline
      } else {
        style.scroll_snap_align.block
      };
      if let Some(pos) = snap_position(
        align,
        rel_bounds.min_y(),
        rel_bounds.max_y(),
        self.viewport.height,
        self.padding_y.0,
        self.padding_y.1,
        margin_start,
        margin_end,
        self.start_positive_y,
      ) {
        self.targets_y.push(ScrollSnapTarget {
          box_id: fragment_box_id(node),
          position: pos,
          stop: style.scroll_snap_stop,
        });
      }
    }
  }

  fn finalize(self) -> ScrollSnapContainer {
    let targets_x = self.targets_x;
    let targets_y = self.targets_y;

    let mut min_x = self.content_bounds.min_x();
    let mut max_x = self.content_bounds.max_x();
    if self.snap_x {
      if let Some(max_target_x) = targets_x
        .iter()
        .map(|t| t.position)
        .filter(|p| p.is_finite())
        .max_by(|a, b| a.total_cmp(b))
      {
        max_x = max_x.max(max_target_x + self.viewport.width);
      }
      if let Some(min_target_x) = targets_x
        .iter()
        .map(|t| t.position)
        .filter(|p| p.is_finite())
        .min_by(|a, b| a.total_cmp(b))
      {
        min_x = min_x.min(min_target_x);
      }
    }

    let mut min_y = self.content_bounds.min_y();
    let mut max_y = self.content_bounds.max_y();
    if self.snap_y {
      if let Some(max_target_y) = targets_y
        .iter()
        .map(|t| t.position)
        .filter(|p| p.is_finite())
        .max_by(|a, b| a.total_cmp(b))
      {
        max_y = max_y.max(max_target_y + self.viewport.height);
      }
      if let Some(min_target_y) = targets_y
        .iter()
        .map(|t| t.position)
        .filter(|p| p.is_finite())
        .min_by(|a, b| a.total_cmp(b))
      {
        min_y = min_y.min(min_target_y);
      }
    }

    let scroll_bounds = Rect::from_xywh(
      min_x,
      min_y,
      (max_x - min_x).max(0.0),
      (max_y - min_y).max(0.0),
    );

    ScrollSnapContainer {
      box_id: self.box_id,
      viewport: self.viewport,
      strictness: self.strictness,
      behavior: self.behavior,
      snap_x: self.snap_x,
      snap_y: self.snap_y,
      axis_is_inline_for_x: self.axis_is_inline_for_x,
      axis_is_inline_for_y: self.axis_is_inline_for_y,
      padding_x: self.padding_x,
      padding_y: self.padding_y,
      scroll_bounds,
      targets_x,
      targets_y,
      uses_viewport_scroll: self.uses_viewport_scroll,
    }
  }
}

fn collect_scroll_metadata(
  node: &mut FragmentNode,
  origin: Point,
  stack: &mut Vec<PendingContainer>,
  metadata: &mut ScrollMetadata,
  root_viewport: Size,
  viewport_container: Option<Option<usize>>,
  has_fixed_cb_ancestor: bool,
  active_container_start: usize,
) {
  let style = node.style.clone();
  let establishes_fixed_cb = style
    .as_deref()
    .is_some_and(|style| style.establishes_fixed_containing_block());
  let is_viewport_fixed = style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
    && !has_fixed_cb_ancestor;
  let has_fixed_cb_ancestor = has_fixed_cb_ancestor || establishes_fixed_cb;
  let active_container_start = if is_viewport_fixed {
    stack.len()
  } else {
    active_container_start
  };

  let mut pushed = false;
  if let Some(style) = style.as_ref() {
    let uses_viewport_scroll =
      stack.is_empty() && viewport_container == Some(fragment_box_id(node));
    let viewport = if uses_viewport_scroll {
      root_viewport
    } else {
      Size::new(node.bounds.width(), node.bounds.height())
    };
    if let Some(container) =
      PendingContainer::new(node, style, origin, uses_viewport_scroll, viewport)
    {
      stack.push(container);
      pushed = true;
    }
  }

  // `PendingContainer::new` seeds `content_bounds` from the container fragment's already-computed
  // `scroll_overflow` (which includes all descendants with intermediate overflow clipping applied).
  //
  // We intentionally do *not* union every descendant fragment's `scroll_overflow` into each
  // ancestor container here. `scroll_overflow` is stored in each fragment's local coordinate space
  // and does not account for clipping imposed by its ancestors, so naively translating+unioning it
  // would re-introduce geometry that should have been clipped and inflate scroll snap bounds.

  if let Some(style) = style.as_ref() {
    for container in stack.iter_mut().skip(active_container_start) {
      container.collect_targets(style, node, origin);
    }
  }

  for child in node.children_mut() {
    let child_origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
    collect_scroll_metadata(
      child,
      child_origin,
      stack,
      metadata,
      root_viewport,
      viewport_container,
      has_fixed_cb_ancestor,
      active_container_start,
    );
  }

  if pushed {
    if let Some(container) = stack.pop() {
      metadata.containers.push(container.finalize());
    }
  }
}

fn dedup_snap_targets(targets: &mut Vec<ScrollSnapTarget>) {
  use std::collections::hash_map::Entry;
  let mut seen: HashMap<(Option<usize>, u32), usize> = HashMap::new();
  let mut out: Vec<ScrollSnapTarget> = Vec::with_capacity(targets.len());
  for target in targets.drain(..) {
    let key = (target.box_id, target.position.to_bits());
    match seen.entry(key) {
      Entry::Occupied(entry) => {
        let existing = &mut out[*entry.get()];
        if matches!(target.stop, ScrollSnapStop::Always) {
          existing.stop = ScrollSnapStop::Always;
        }
      }
      Entry::Vacant(entry) => {
        entry.insert(out.len());
        out.push(target);
      }
    }
  }
  *targets = out;
}

fn merge_containers(containers: Vec<ScrollSnapContainer>) -> Vec<ScrollSnapContainer> {
  let mut merged: Vec<ScrollSnapContainer> = Vec::new();
  let mut by_id: HashMap<usize, usize> = HashMap::new();
  let mut viewport_idx: Option<usize> = None;

  for container in containers {
    let merge_target = if container.uses_viewport_scroll {
      viewport_idx
    } else if let Some(box_id) = container.box_id {
      by_id.get(&box_id).copied()
    } else {
      None
    };

    if let Some(idx) = merge_target {
      let existing = &mut merged[idx];
      debug_assert_eq!(
        existing.uses_viewport_scroll,
        container.uses_viewport_scroll
      );
      debug_assert_eq!(
        existing.axis_is_inline_for_x,
        container.axis_is_inline_for_x
      );
      debug_assert_eq!(
        existing.axis_is_inline_for_y,
        container.axis_is_inline_for_y
      );
      existing.strictness = match (existing.strictness, container.strictness) {
        (ScrollSnapStrictness::Mandatory, _) | (_, ScrollSnapStrictness::Mandatory) => {
          ScrollSnapStrictness::Mandatory
        }
        _ => ScrollSnapStrictness::Proximity,
      };
      existing.behavior = match (existing.behavior, container.behavior) {
        (ScrollBehavior::Smooth, _) | (_, ScrollBehavior::Smooth) => ScrollBehavior::Smooth,
        _ => ScrollBehavior::Auto,
      };
      existing.snap_x |= container.snap_x;
      existing.snap_y |= container.snap_y;
      existing.viewport = Size::new(
        existing.viewport.width.max(container.viewport.width),
        existing.viewport.height.max(container.viewport.height),
      );
      existing.padding_x = (
        existing.padding_x.0.max(container.padding_x.0),
        existing.padding_x.1.max(container.padding_x.1),
      );
      existing.padding_y = (
        existing.padding_y.0.max(container.padding_y.0),
        existing.padding_y.1.max(container.padding_y.1),
      );

      existing.scroll_bounds = existing.scroll_bounds.union(container.scroll_bounds);
      existing.targets_x.extend(container.targets_x);
      existing.targets_y.extend(container.targets_y);
    } else {
      let idx = merged.len();
      if container.uses_viewport_scroll {
        viewport_idx = Some(idx);
      } else if let Some(box_id) = container.box_id {
        by_id.insert(box_id, idx);
      }
      merged.push(container);
    }
  }

  for container in &mut merged {
    dedup_snap_targets(&mut container.targets_x);
    dedup_snap_targets(&mut container.targets_y);
  }

  merged
}

/// Computes scrollable overflow areas and snap target lists for a fragment tree.
pub(crate) fn build_scroll_metadata(tree: &mut FragmentTree) -> ScrollMetadata {
  let is_snap_container = |node: &FragmentNode| {
    node
      .style
      .as_ref()
      .map(|style| {
        let inline_vertical = is_vertical_writing_mode(style.writing_mode);
        let (snap_x, snap_y) = snap_axis_flags(style.scroll_snap_type.axis, inline_vertical);
        snap_x || snap_y
      })
      .unwrap_or(false)
  };

  // The viewport scroll snap container is determined by the document's root scrolling element.
  //
  // In non-fragmented trees, this is typically `tree.root` (the HTML element), with the `<body>`
  // nested under the first child. In paginated trees, the root is the synthetic page box, with the
  // document root nested under the first child.
  //
  // Walk down a few nodes along the first-child chain so paginated trees can still locate the
  // document root. We intentionally *do not* search for the first snap container here: doing so
  // would mis-classify an element scroller near the top of the document as the viewport snap
  // container, causing scroll snap to consult viewport scroll offsets instead of element scroll
  // offsets.
  let viewport_container = {
    let mut current: Option<&FragmentNode> = Some(&tree.root);
    let mut found: Option<Option<usize>> = None;
    for depth in 0..4 {
      let Some(node) = current else { break };
      let box_id = fragment_box_id(node);
      // Prefer the first fragment that corresponds to a real box (i.e. has a stable box id). This
      // is typically the document root element (HTML) even when a synthetic wrapper (e.g. a page
      // box) sits above it.
      //
      // When a synthetic tree omits box ids for the document root (common in unit tests), the
      // first encountered box id can belong to an arbitrary descendant element scroller. Avoid
      // promoting those deeper descendants to the viewport snap container by only accepting box ids
      // within the first synthetic wrapper layer.
      if box_id.is_some() {
        if depth <= 1 {
          found = Some(box_id);
        }
        break;
      }
      // If we never encounter a fragment with a box id (e.g. a synthetic root in tests), keep the
      // root snap container using viewport scroll offsets when it is itself a snap container.
      if depth == 0 && is_snap_container(node) {
        found = Some(box_id);
        break;
      }
      // `ScrollState.viewport` corresponds to the fragment tree root (the viewport scroll container).
      // Only descend into synthetic roots (e.g. paged-media page boxes with `box_id=None`) to find
      // the real document root. Avoid promoting arbitrary descendant element scrollers that happen
      // to be the first child of `<body>` etc.
      if fragment_box_id(node).is_some() {
        break;
      }
      current = node.children.iter().next();
    }
    found
  };

  let mut metadata = ScrollMetadata::default();
  let mut stack = Vec::new();
  let root_viewport = tree.viewport_size();
  annotate_overflow(&mut tree.root, false, root_viewport);
  for fragment in &mut tree.additional_fragments {
    annotate_overflow(fragment, false, root_viewport);
  }
  collect_scroll_metadata(
    &mut tree.root,
    Point::ZERO,
    &mut stack,
    &mut metadata,
    root_viewport,
    viewport_container,
    false,
    0,
  );

  for fragment in &mut tree.additional_fragments {
    collect_scroll_metadata(
      fragment,
      Point::new(fragment.bounds.x(), fragment.bounds.y()),
      &mut stack,
      &mut metadata,
      root_viewport,
      viewport_container,
      false,
      0,
    );
  }

  metadata.containers = merge_containers(metadata.containers);
  metadata
}

fn snap_axis(
  current: f32,
  viewport_extent: f32,
  strictness: ScrollSnapStrictness,
  targets: &[ScrollSnapTarget],
  bounds: &Rect,
  vertical: bool,
) -> f32 {
  if !current.is_finite() || !viewport_extent.is_finite() {
    return current;
  }

  let origin = if vertical {
    bounds.min_y()
  } else {
    bounds.min_x()
  };
  if !origin.is_finite() {
    return current;
  }

  let bounds_max = if vertical {
    bounds.max_y()
  } else {
    bounds.max_x()
  };
  if !bounds_max.is_finite() {
    return current;
  }

  let max_scroll = if vertical {
    (bounds_max - origin - viewport_extent).max(0.0)
  } else {
    (bounds_max - origin - viewport_extent).max(0.0)
  };
  if !max_scroll.is_finite() {
    return current;
  }

  let candidates: Vec<(f32, ScrollSnapStop)> = targets
    .iter()
    .filter_map(|t| {
      let pos = t.position - origin;
      pos.is_finite().then_some((pos, t.stop))
    })
    .collect();
  pick_snap_target(
    current - origin,
    max_scroll,
    strictness,
    viewport_extent * 0.5,
    &candidates,
  ) + origin
}

/// Applies scroll snap to all snap containers in the fragment tree.
pub fn apply_scroll_snap(tree: &mut FragmentTree, scroll: &ScrollState) -> ScrollSnapResult {
  tree.ensure_scroll_metadata();
  let Some(metadata) = tree.scroll_metadata.as_ref() else {
    return ScrollSnapResult {
      state: scroll.clone(),
      updates: Vec::new(),
    };
  };

  apply_scroll_snap_from_metadata(metadata, scroll)
}

/// Applies scroll snap to a scroll state using precomputed scroll metadata.
///
/// This is a lightweight alternative to [`apply_scroll_snap`] for callers that already have access
/// to a fragment tree's [`ScrollMetadata`] (for example, UI workers performing high-frequency scroll
/// acknowledgements).
///
/// Note: This helper does **not** compute missing metadata. Callers that need scroll snap on
/// synthetic fragment trees should use [`apply_scroll_snap`] or call
/// [`FragmentTree::ensure_scroll_metadata`] first.
pub fn apply_scroll_snap_from_metadata(
  metadata: &ScrollMetadata,
  scroll: &ScrollState,
) -> ScrollSnapResult {
  let mut state = scroll.clone();
  let mut updates = Vec::new();

  for container in &metadata.containers {
    let element_offset = container
      .box_id
      .and_then(|id| state.elements.get(&id).copied());
    if !container.uses_viewport_scroll && element_offset.is_none() {
      continue;
    }

    let mut current = if container.uses_viewport_scroll {
      state.viewport
    } else if let Some(offset) = element_offset {
      offset
    } else {
      state.viewport
    };

    if container.snap_x {
      current.x = snap_axis(
        current.x,
        container.viewport.width,
        container.strictness,
        &container.targets_x,
        &container.scroll_bounds,
        false,
      );
    }
    if container.snap_y {
      current.y = snap_axis(
        current.y,
        container.viewport.height,
        container.strictness,
        &container.targets_y,
        &container.scroll_bounds,
        true,
      );
    }

    let changed;
    if container.uses_viewport_scroll {
      changed = current != state.viewport;
      state.viewport = current;
    } else if let Some(id) = container.box_id {
      let prev = state.elements.insert(id, current);
      changed = prev.map(|p| p != current).unwrap_or(true);
    } else {
      changed = current != state.viewport;
      state.viewport = current;
    }

    if changed {
      updates.push(ScrollSnapUpdate {
        container: container.box_id,
        offset: current,
        behavior: container.behavior,
      });
    }
  }

  ScrollSnapResult { state, updates }
}

/// Resolve the scroll state that the paint pipeline will actually apply for a given request.
///
/// Painting performs some scroll adjustments before any rasterization happens:
/// - CSS scroll snap may shift the viewport or element scroll offsets.
/// - Viewport scroll is sanitized and clamped to the root scroll bounds.
///
/// This helper mirrors that logic without applying scroll translations or rasterizing, so callers
/// (e.g. scroll-blit fast paths) can compute the *effective* scroll offset and resulting integer
/// device-pixel deltas.
///
/// Note: this currently mirrors the pre-paint adjustments performed in
/// `paint_fragment_tree_with_state` (see `src/api.rs`).
pub fn resolve_effective_scroll_state_for_paint(
  fragment_tree: &FragmentTree,
  scroll_state: ScrollState,
  scrollport_viewport: Size,
) -> ScrollState {
  let mut tree = fragment_tree.clone();
  resolve_effective_scroll_state_for_paint_mut(&mut tree, scroll_state, scrollport_viewport)
}

pub(crate) fn resolve_effective_scroll_state_for_paint_mut(
  fragment_tree: &mut FragmentTree,
  scroll_state: ScrollState,
  scrollport_viewport: Size,
) -> ScrollState {
  let scroll_result = apply_scroll_snap(fragment_tree, &scroll_state);
  let mut scroll_state = scroll_result.state;

  // Clamp/sanitize viewport scroll offsets even when they were set programmatically. This keeps the
  // paint pipeline consistent with user-driven scrolling (wheel/anchor), and prevents
  // viewport-fixed descendants from incorrectly inflating the scroll range.
  scroll_state.viewport = Point::new(
    if scroll_state.viewport.x.is_finite() {
      scroll_state.viewport.x
    } else {
      0.0
    },
    if scroll_state.viewport.y.is_finite() {
      scroll_state.viewport.y
    } else {
      0.0
    },
  );

  scroll_state.viewport = viewport_scroll_bounds(&fragment_tree.root, scrollport_viewport)
    .clamp(scroll_state.viewport);

  scroll_state
}

fn apply_element_scroll_offsets(
  node: &mut FragmentNode,
  scroll: &ScrollState,
  cumulative_translation: Point,
  has_fixed_cb_ancestor: bool,
) {
  let (establishes_fixed_cb, is_viewport_fixed) = node
    .style
    .as_deref()
    .map(|style| {
      (
        style.establishes_fixed_containing_block(),
        matches!(style.position, Position::Fixed) && !has_fixed_cb_ancestor,
      )
    })
    .unwrap_or((false, false));
  let (cumulative_translation, has_fixed_cb_ancestor) = if is_viewport_fixed {
    if cumulative_translation != Point::ZERO {
      node.translate_root_in_place(Point::new(
        -cumulative_translation.x,
        -cumulative_translation.y,
      ));
    }
    (Point::ZERO, false)
  } else {
    (cumulative_translation, has_fixed_cb_ancestor)
  };

  // Only apply element scroll offsets for real scroll containers.
  //
  // CSS Overflow 3:
  // - `overflow: hidden|scroll|auto` are scroll containers (hidden allows programmatic scrolling).
  // - `overflow: visible|clip` are not scroll containers, and `clip` forbids scrolling entirely.
  //
  // `ScrollState::elements` is a generic (id -> offset) map that callers can populate arbitrarily;
  // enforce the spec rule here so non-scrollable boxes cannot be scrolled by accident.
  let offset = fragment_box_id(node)
    .and_then(|id| {
      let style = node.style.as_deref()?;
      let mut offset = scroll.elements.get(&id).copied().unwrap_or(Point::ZERO);
      if !matches!(
        style.overflow_x,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto
      ) {
        offset.x = 0.0;
      }
      if !matches!(
        style.overflow_y,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto
      ) {
        offset.y = 0.0;
      }
      (offset != Point::ZERO).then_some(offset)
    })
    .unwrap_or(Point::ZERO);
  let delta = Point::new(-offset.x, -offset.y);
  let mut child_cumulative_translation = cumulative_translation;
  if offset != Point::ZERO {
    for child in node.children_mut() {
      child.translate_root_in_place(delta);
    }
    child_cumulative_translation = Point::new(
      child_cumulative_translation.x + delta.x,
      child_cumulative_translation.y + delta.y,
    );
  }

  let has_fixed_cb_ancestor_for_children = has_fixed_cb_ancestor || establishes_fixed_cb;
  for child in node.children_mut() {
    apply_element_scroll_offsets(
      child,
      scroll,
      child_cumulative_translation,
      has_fixed_cb_ancestor_for_children,
    );
  }
}

/// Applies element scroll offsets to a fragment tree by translating the contents of each scroll
/// container by the corresponding offset in the [`ScrollState`].
///
/// The tree is mutated in place and should typically be a clone of a prepared layout tree to avoid
/// leaking state across paints.
pub fn apply_scroll_offsets(tree: &mut FragmentTree, scroll: &ScrollState) {
  apply_element_scroll_offsets(&mut tree.root, scroll, Point::ZERO, false);
  for fragment in &mut tree.additional_fragments {
    apply_element_scroll_offsets(fragment, scroll, Point::ZERO, false);
  }
}
fn viewport_rect_for_scroll_state(
  scroll: Point,
  viewport: Size,
  writing_mode: WritingMode,
  direction: Direction,
) -> Rect {
  // The scroll state uses logical-start-relative offsets (clamped to non-negative values). Convert
  // them into a visible viewport rectangle expressed in the fragment tree's physical coordinate
  // space.
  //
  // When the logical start edge for an axis is on the opposite side (e.g. RTL inline axis or
  // `writing-mode: vertical-rl` block axis), increasing the scroll offset moves the visible window
  // towards negative coordinates, so the viewport origin becomes `-scroll`.
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

  let origin_x = if x_positive { scroll.x } else { -scroll.x };
  let origin_y = if y_positive { scroll.y } else { -scroll.y };
  Rect::from_xywh(origin_x, origin_y, viewport.width, viewport.height)
}

/// Convenience wrapper for applying scroll anchoring between two layout results.
///
/// This function captures scroll anchors from `prev` and then applies them to `next`, returning the
/// adjusted scroll state. Callers that need to keep the updated snapshot should use
/// [`apply_scroll_anchoring`] directly.
pub fn apply_scroll_anchoring_between_fragment_trees(
  prev: &FragmentTree,
  next: &FragmentTree,
  scroll: &ScrollState,
) -> ScrollState {
  let snapshot = capture_scroll_anchors(prev, scroll);
  let (anchored, _next_snapshot) = apply_scroll_anchoring(&snapshot, next, scroll);
  anchored
}
fn apply_viewport_scroll_cancel_to_fixed(
  node: &mut FragmentNode,
  viewport_scroll: Point,
  skip_viewport_scroll_cancel: bool,
) {
  let establishes_fixed_cb = node
    .style
    .as_deref()
    .is_some_and(|style| style.establishes_fixed_containing_block());
  let needs_viewport_scroll_cancel = node
    .style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
    && !skip_viewport_scroll_cancel;

  if needs_viewport_scroll_cancel {
    node.translate_root_in_place(viewport_scroll);
  }

  // Mirror paint-time `skip_viewport_scroll_cancel` propagation:
  // - Once a viewport-scroll cancel has been applied to a fixed subtree, descendants should not
  //   apply it again (nested fixed elements would otherwise cancel twice).
  // - Descendants of a fixed-containing-block establish their own coordinate space, so fixed
  //   elements within that subtree are not viewport-fixed and must not cancel viewport scroll.
  let skip_for_children =
    skip_viewport_scroll_cancel || establishes_fixed_cb || needs_viewport_scroll_cancel;
  for child in node.children_mut() {
    apply_viewport_scroll_cancel_to_fixed(child, viewport_scroll, skip_for_children);
  }
}

/// Applies viewport-scroll cancel semantics to `position: fixed` fragments.
///
/// Layout positions viewport-fixed fragments in viewport-relative coordinates. When hit-testing in
/// page coordinates (i.e. `page_point = viewport_point + scroll.viewport`), those fixed fragments
/// must be translated into page space so hit testing mirrors paint-time geometry.
///
/// This mirrors the painter's `needs_viewport_scroll_cancel` / `skip_viewport_scroll_cancel` logic
/// to respect fixed-containing-block semantics and to avoid double-applying the viewport scroll
/// offset for nested fixed elements.
pub fn apply_viewport_scroll_cancel(tree: &mut FragmentTree, scroll: &ScrollState) {
  let viewport_scroll = if scroll.viewport.x.is_finite() && scroll.viewport.y.is_finite() {
    scroll.viewport
  } else {
    Point::ZERO
  };

  if viewport_scroll == Point::ZERO {
    return;
  }

  apply_viewport_scroll_cancel_to_fixed(&mut tree.root, viewport_scroll, false);
  for fragment in tree.additional_fragments.iter_mut() {
    apply_viewport_scroll_cancel_to_fixed(fragment, viewport_scroll, false);
  }
}

/// Returns `true` if a fragment tree can safely use the "scroll blit" fast-path.
///
/// Scroll blitting (reusing the previously rendered frame by shifting pixels) is only valid when
/// the entire rendered output is translated uniformly by viewport scroll.
///
/// This scan is intentionally conservative: it disables scroll blitting for known scroll-breaking
/// features that can keep parts of the page anchored to the viewport or otherwise depend on scroll
/// position.
pub(crate) fn scroll_blit_supported(tree: &FragmentTree) -> bool {
  // Avoid recursion to prevent stack overflows on adversarially deep fragment trees.
  let mut stack: Vec<(&FragmentNode, bool)> = Vec::new();
  stack.push((&tree.root, false));
  for fragment in tree.additional_fragments.iter() {
    stack.push((fragment, false));
  }

  while let Some((node, has_fixed_cb_ancestor)) = stack.pop() {
    match &node.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        stack.push((snapshot, has_fixed_cb_ancestor));
      }
      _ => {}
    }

    let Some(style) = node.style.as_deref() else {
      for child in node.children.iter() {
        stack.push((child, has_fixed_cb_ancestor));
      }
      continue;
    };

    if crate::paint::scroll_blit::style_uses_scroll_linked_timelines(style)
      || node
        .starting_style
        .as_deref()
        .is_some_and(crate::paint::scroll_blit::style_uses_scroll_linked_timelines)
    {
      return false;
    }

    // Sticky positioning is scroll-dependent; treat any sticky as unsupported for now.
    if matches!(style.position, Position::Sticky) {
      return false;
    }

    // A `position: fixed` element is only viewport-fixed when it has no fixed-containing-block
    // ancestor. This mirrors the logic in `apply_element_scroll_offsets`.
    if matches!(style.position, Position::Fixed) && !has_fixed_cb_ancestor {
      return false;
    }

    // `background-attachment: fixed` keeps the background anchored to the viewport.
    if style
      .background_layers
      .iter()
      .any(|layer| matches!(layer.attachment, BackgroundAttachment::Fixed))
    {
      return false;
    }

    let establishes_fixed_cb = style.establishes_fixed_containing_block();
    let has_fixed_cb_ancestor_for_children = has_fixed_cb_ancestor || establishes_fixed_cb;
    for child in node.children.iter() {
      stack.push((child, has_fixed_cb_ancestor_for_children));
    }
  }

  true
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollBounds {
  pub min_x: f32,
  pub min_y: f32,
  pub max_x: f32,
  pub max_y: f32,
}

impl ScrollBounds {
  pub fn clamp(&self, scroll: Point) -> Point {
    let clamp_axis = |value: f32, min: f32, max: f32| {
      if value.is_finite() && min.is_finite() && max.is_finite() && min <= max {
        value.clamp(min, max)
      } else {
        value
      }
    };

    Point::new(
      clamp_axis(scroll.x, self.min_x, self.max_x),
      clamp_axis(scroll.y, self.min_y, self.max_y),
    )
  }
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
  min_x: f32,
  min_y: f32,
  max_x: f32,
  max_y: f32,
}

impl Bounds {
  fn new(rect: Rect) -> Self {
    Self {
      min_x: rect.min_x(),
      min_y: rect.min_y(),
      max_x: rect.max_x(),
      max_y: rect.max_y(),
    }
  }

  fn update(&mut self, rect: Rect) {
    self.min_x = self.min_x.min(rect.min_x());
    self.min_y = self.min_y.min(rect.min_y());
    self.max_x = self.max_x.max(rect.max_x());
    self.max_y = self.max_y.max(rect.max_y());
  }
}

#[derive(Debug, Clone, Copy)]
struct AxisClipRect {
  min_x: f32,
  max_x: f32,
  min_y: f32,
  max_y: f32,
}

impl AxisClipRect {
  fn unbounded() -> Self {
    Self {
      min_x: f32::NEG_INFINITY,
      max_x: f32::INFINITY,
      min_y: f32::NEG_INFINITY,
      max_y: f32::INFINITY,
    }
  }

  fn intersect_rect(self, rect: Rect) -> Option<Rect> {
    let min_x = rect.min_x().max(self.min_x);
    let max_x = rect.max_x().min(self.max_x);
    let min_y = rect.min_y().max(self.min_y);
    let max_y = rect.max_y().min(self.max_y);

    if max_x < min_x || max_y < min_y {
      return None;
    }

    Some(Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y))
  }

  fn clip_to_rect_on_axes(self, rect: Rect, clip_x: bool, clip_y: bool) -> Option<Self> {
    let mut out = self;

    if clip_x {
      out.min_x = out.min_x.max(rect.min_x());
      out.max_x = out.max_x.min(rect.max_x());
      if out.max_x < out.min_x {
        return None;
      }
    }

    if clip_y {
      out.min_y = out.min_y.max(rect.min_y());
      out.max_y = out.max_y.min(rect.max_y());
      if out.max_y < out.min_y {
        return None;
      }
    }

    Some(out)
  }
}
fn collect_bounds(
  node: &FragmentNode,
  origin: Point,
  bounds: &mut Bounds,
  clip: AxisClipRect,
  root: bool,
  viewport: Size,
  transform: Transform3D,
  has_fixed_cb_ancestor: bool,
) {
  // Viewport-fixed descendants do not participate in scrollable overflow for any ancestor scroll
  // container, since they remain pinned to the viewport (unless a fixed containing block is
  // established).
  if node
    .style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
    && !has_fixed_cb_ancestor
  {
    return;
  }

  let rect = Rect::from_xywh(
    origin.x,
    origin.y,
    node.bounds.width(),
    node.bounds.height(),
  );

  let next_transform = node.style.as_deref().and_then(|style| {
    if style.has_transform() || style.has_motion_path() {
      crate::paint::transform_resolver::resolve_transform3d(
        style,
        rect,
        Some((viewport.width, viewport.height)),
      )
    } else {
      None
    }
  });
  let transform = if let Some(next_transform) = next_transform {
    transform.multiply(&next_transform)
  } else {
    transform
  };
  let rect = transform.transform_rect(rect);

  // When bubbling descendant bounds into ancestor scroll containers, apply clipping established by
  // intermediate overflow/clip ancestors. Overflow clipping uses the scrollport (padding edge minus
  // any reserved scrollbar gutters), while CSS2.1 `clip: rect()` applies to the border box.
  let mut node_clip = clip;
  if !root {
    if let Some(style) = node.style.as_ref() {
      let clip_x = overflow_axis_clips(style.overflow_x);
      let clip_y = overflow_axis_clips(style.overflow_y);
      if clip_x || clip_y {
        let local_clip = overflow_clip_rect(node, style, viewport);
        let clip_rect = transform.transform_rect(local_clip.translate(origin));
        let Some(updated) = node_clip.clip_to_rect_on_axes(clip_rect, clip_x, clip_y) else {
          return;
        };
        node_clip = updated;
      }

      if let Some(local_clip) = clip_rect_from_style(node, style, viewport) {
        let clip_rect = transform.transform_rect(local_clip.translate(origin));
        let Some(updated) = node_clip.clip_to_rect_on_axes(clip_rect, true, true) else {
          return;
        };
        node_clip = updated;
      }
    }
  }

  let Some(visible_rect) = node_clip.intersect_rect(rect) else {
    return;
  };
  bounds.update(visible_rect);

  let child_clip = node_clip;

  let has_fixed_cb_ancestor = has_fixed_cb_ancestor
    || node
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_fixed_containing_block());

  for child in node.children.iter() {
    let child_origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
    collect_bounds(
      child,
      child_origin,
      bounds,
      child_clip,
      false,
      viewport,
      transform,
      has_fixed_cb_ancestor,
    );
  }
}

/// Returns scroll bounds for viewport scrolling (i.e. `ScrollState.viewport`).
///
/// This is equivalent to `build_scroll_chain(root, viewport, &[]).first().map(|s| s.bounds)` but
/// avoids the intermediate `Vec` allocation.
pub(crate) fn viewport_scroll_bounds(root: &FragmentNode, viewport: Size) -> ScrollBounds {
  scroll_bounds_for_fragment(
    root,
    Point::new(root.bounds.x(), root.bounds.y()),
    viewport,
    viewport,
    /* treat_as_root */ true,
    /* has_fixed_cb_ancestor */ false,
  )
}

pub(crate) fn scroll_bounds_for_fragment(
  container: &FragmentNode,
  _origin: Point,
  viewport: Size,
  _viewport_for_units: Size,
  treat_as_root: bool,
  _has_fixed_cb_ancestor: bool,
) -> ScrollBounds {
  // Scroll bounds are defined over the scrollport (the viewport through which descendants are
  // scrolled). `FragmentNode::bounds` describes the element's border box, so we must account for:
  // - border widths (scrollport is the padding box, not the border box), and
  // - any reserved scrollbar gutters (`scrollbar_reservation`), which behave like additional
  //   padding and shrink the available scrollport.
  //
  // For element scrollers we compute the actual scrollport rectangle and shift the coordinate
  // space so the scrollport origin is treated as (0, 0). This keeps scroll offsets expressed in
  // the same local coordinate system used by painting/layout (i.e. `scroll=0` means content is
  // aligned with the scrollport start edge) even when borders/gutters offset the scrollport within
  // the fragment's border box.
  let (scrollport_origin, viewport) = if treat_as_root {
    // The root scroll container uses the passed `viewport` size (the layout scrollport viewport),
    // so only reserved scrollbar gutters apply here.
    let reservation = container.scrollbar_reservation;
    let reserve_left = sanitize_nonneg(reservation.left);
    let reserve_right = sanitize_nonneg(reservation.right);
    let reserve_top = sanitize_nonneg(reservation.top);
    let reserve_bottom = sanitize_nonneg(reservation.bottom);
    (
      Point::new(reserve_left, reserve_top),
      Size::new(
        (viewport.width - reserve_left - reserve_right).max(0.0),
        (viewport.height - reserve_top - reserve_bottom).max(0.0),
      ),
    )
  } else if let Some(style) = container.style.as_deref() {
    let rect = scrollport_rect_for_fragment(container, style);
    (rect.origin, rect.size)
  } else {
    // Fallback for synthetic fragment trees without styles: assume no borders, but still honor any
    // scrollbar reservation attached to the fragment.
    let reservation = container.scrollbar_reservation;
    let reserve_left = sanitize_nonneg(reservation.left);
    let reserve_right = sanitize_nonneg(reservation.right);
    let reserve_top = sanitize_nonneg(reservation.top);
    let reserve_bottom = sanitize_nonneg(reservation.bottom);
    (
      Point::new(reserve_left, reserve_top),
      Size::new(
        (viewport.width - reserve_left - reserve_right).max(0.0),
        (viewport.height - reserve_top - reserve_bottom).max(0.0),
      ),
    )
  };

  let mut bounds = Bounds::new(Rect::from_xywh(0.0, 0.0, viewport.width, viewport.height));

  // For the root scroll container (viewport scrolling), include the root fragment's border box in
  // the content bounds when it is larger than the scrollport (e.g. in tests that use the root
  // bounds to encode document content size). For non-root element scrollers we intentionally do
  // not union the border box because it can include reserved scrollbar gutters, which would
  // incorrectly allow scrolling even when the scrollable contents do not overflow.
  if treat_as_root
    && container.scrollbar_reservation
      == crate::tree::fragment_tree::ScrollbarReservation::default()
  {
    bounds.update(Rect::from_xywh(
      0.0,
      0.0,
      container.bounds.width(),
      container.bounds.height(),
    ));
  }

  // Layout computes `FragmentNode::scroll_overflow` (via `FragmentTree::ensure_scroll_metadata`)
  // which already accounts for transforms and intermediate overflow clipping. Avoid re-traversing
  // the fragment subtree here and instead translate the precomputed overflow into scrollport-local
  // coordinates.
  let overflow = container.scroll_overflow;

  if overflow.min_x().is_finite() && overflow.min_x() < 0.0 {
    bounds.min_x = bounds.min_x.min(overflow.min_x() - scrollport_origin.x);
  }
  if overflow.min_y().is_finite() && overflow.min_y() < 0.0 {
    bounds.min_y = bounds.min_y.min(overflow.min_y() - scrollport_origin.y);
  }

  let overflow_max_x = overflow.max_x();
  if overflow_max_x.is_finite() {
    let translated_max_x = overflow_max_x - scrollport_origin.x;
    if treat_as_root
      && container.scrollbar_reservation
        == crate::tree::fragment_tree::ScrollbarReservation::default()
    {
      bounds.max_x = bounds.max_x.max(translated_max_x);
    } else if container.bounds.width().is_finite() && overflow_max_x > container.bounds.width() {
      // Ignore the container's own border box when it is larger than the scrollport (borders +
      // reserved gutters), but preserve descendant overflow that actually extends beyond the
      // border box.
      bounds.max_x = bounds.max_x.max(translated_max_x);
    }
  }

  let overflow_max_y = overflow.max_y();
  if overflow_max_y.is_finite() {
    let translated_max_y = overflow_max_y - scrollport_origin.y;
    if treat_as_root
      && container.scrollbar_reservation
        == crate::tree::fragment_tree::ScrollbarReservation::default()
    {
      bounds.max_y = bounds.max_y.max(translated_max_y);
    } else if container.bounds.height().is_finite() && overflow_max_y > container.bounds.height() {
      bounds.max_y = bounds.max_y.max(translated_max_y);
    }
  }

  // Scroll offsets are expressed relative to the scrollport start edge (the padding edge after
  // accounting for borders/gutters). Browsers clamp scroll offsets to non-negative values.
  //
  // Historically we ignored any content that overflowed to the left/top (e.g. `text-indent:-9999px`
  // visually-hidden labels) when computing scrollable ranges. That matches typical LTR flow where
  // the scroll origin is aligned to the top-left of the scrollport.
  //
  // However, in writing modes where the logical start edge for an axis is on the *opposite* side
  // (e.g. `writing-mode: vertical-rl` has a right-to-left block axis), layout can legitimately
  // position content at negative coordinates in order to align to that start edge. In those cases
  // we still need a non-zero scroll range so scroll-state container queries (and scrolling itself)
  // can observe overflow.
  //
  // To keep scroll offsets non-negative (matching our scroll state model), we fold negative
  // overflow into the effective content extent only when the corresponding logical axis has a
  // "negative" progression direction.
  let min_x = 0.0;
  let min_y = 0.0;
  let mut content_min_x = 0.0;
  let mut content_min_y = 0.0;
  if let Some(style) = container.style.as_deref() {
    let wm = style.writing_mode;
    let dir = style.direction;
    // Physical X is the horizontal axis; it corresponds to the inline axis in horizontal writing
    // modes and the block axis in vertical writing modes.
    let x_is_inline = crate::style::inline_axis_is_horizontal(wm);
    let x_positive = if x_is_inline {
      crate::style::inline_axis_positive(wm, dir)
    } else {
      crate::style::block_axis_positive(wm)
    };
    if !x_positive {
      content_min_x = bounds.min_x.min(0.0);
    }

    // Physical Y is the vertical axis; it corresponds to the block axis in horizontal writing
    // modes and the inline axis in vertical writing modes.
    let y_is_inline = !x_is_inline;
    let y_positive = if y_is_inline {
      crate::style::inline_axis_positive(wm, dir)
    } else {
      crate::style::block_axis_positive(wm)
    };
    if !y_positive {
      content_min_y = bounds.min_y.min(0.0);
    }
  }
  let content_width = (bounds.max_x - content_min_x).max(0.0);
  let content_height = (bounds.max_y - content_min_y).max(0.0);
  let max_x = (content_width - viewport.width).max(min_x);
  let max_y = (content_height - viewport.height).max(min_y);

  ScrollBounds {
    min_x,
    min_y,
    max_x,
    max_y,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
  User,
  Programmatic,
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollOptions {
  pub source: ScrollSource,
  pub simulate_overscroll: bool,
  /// When true, scroll-snap is applied immediately while mutating scroll offsets.
  ///
  /// Most scroll interactions should leave this enabled, but wheel scrolling (especially smooth
  /// trackpad scrolling) should typically accumulate deltas across multiple wheel events and only
  /// apply scroll snapping at paint time (or an explicit "scroll end" step).
  pub apply_snap: bool,
}

impl Default for ScrollOptions {
  fn default() -> Self {
    Self {
      source: ScrollSource::User,
      simulate_overscroll: false,
      apply_snap: true,
    }
  }
}

#[derive(Debug, Clone)]
pub struct ScrollChainSnapInfo<'a> {
  pub container: &'a FragmentNode,
  pub origin: Point,
}

#[derive(Debug, Clone)]
pub struct ScrollChainState<'a> {
  pub container: &'a FragmentNode,
  pub origin: Point,
  pub viewport: Size,
  pub bounds: ScrollBounds,
  pub scroll: Point,
  pub overscroll_behavior_x: OverscrollBehavior,
  pub overscroll_behavior_y: OverscrollBehavior,
  pub snap: Option<ScrollChainSnapInfo<'a>>,
}

impl<'a> ScrollChainState<'a> {
  pub fn from_fragment(
    node: &'a FragmentNode,
    origin: Point,
    viewport: Size,
    viewport_for_units: Size,
    treat_as_root: bool,
    has_fixed_cb_ancestor: bool,
  ) -> Option<Self> {
    let style = node.style.as_ref();
    let overscroll_behavior_x = style
      .map(|s| s.overscroll_behavior_x)
      .unwrap_or(OverscrollBehavior::Auto);
    let overscroll_behavior_y = style
      .map(|s| s.overscroll_behavior_y)
      .unwrap_or(OverscrollBehavior::Auto);
    let overflow_x = style.map(|s| s.overflow_x).unwrap_or(Overflow::Visible);
    let overflow_y = style.map(|s| s.overflow_y).unwrap_or(Overflow::Visible);

    let snap = style.and_then(|s| {
      if s.scroll_snap_type.axis != ScrollSnapAxis::None {
        Some(ScrollChainSnapInfo {
          container: node,
          origin,
        })
      } else {
        None
      }
    });

    // Treat `overflow: hidden` as a scroll container too so keyboard/wheel scrolling (and
    // scroll-state container queries) can observe/programmatically scroll them. This matches the
    // behaviour of `apply_scroll_offsets`, which already honours hidden scroll offsets.
    let scrollable_overflow =
      |overflow: Overflow| matches!(overflow, Overflow::Auto | Overflow::Scroll | Overflow::Hidden);
    let is_scroll_container = treat_as_root
      || scrollable_overflow(overflow_x)
      || scrollable_overflow(overflow_y)
      || snap.is_some();

    if !is_scroll_container {
      return None;
    }

    let bounds = scroll_bounds_for_fragment(
      node,
      origin,
      viewport,
      viewport_for_units,
      treat_as_root,
      has_fixed_cb_ancestor,
    );
    Some(Self {
      container: node,
      origin,
      viewport,
      bounds,
      scroll: Point::ZERO,
      overscroll_behavior_x,
      overscroll_behavior_y,
      snap,
    })
  }
}

/// Builds the scroll chain for a node path.
///
/// When `treat_root_as_scroll_container` is true, the root fragment is always included in the chain
/// even when it is not an overflow scroll container. This is appropriate for document roots where
/// viewport scrolling should always be available.
///
/// When false, the root fragment participates only if it is a real scroll container (overflow
/// scroll/auto or scroll snap). This is useful for additional fragment roots (e.g., fixed layers)
/// that should not be promoted to viewport scroll.
pub fn build_scroll_chain_with_root_mode<'a>(
  root: &'a FragmentNode,
  root_viewport: Size,
  path: &[usize],
  treat_root_as_scroll_container: bool,
) -> Vec<ScrollChainState<'a>> {
  let mut stack: Vec<(&FragmentNode, Point, Size, bool, bool)> = Vec::new();
  let mut current = root;
  let mut origin = Point::new(root.bounds.x(), root.bounds.y());
  let mut current_viewport = root_viewport;
  let mut has_fixed_cb_ancestor = false;
  stack.push((
    current,
    origin,
    current_viewport,
    treat_root_as_scroll_container,
    has_fixed_cb_ancestor,
  ));

  for &idx in path {
    if let Some(child) = current.children.get(idx) {
      has_fixed_cb_ancestor = has_fixed_cb_ancestor
        || current
          .style
          .as_deref()
          .is_some_and(|style| style.establishes_fixed_containing_block());
      origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
      current_viewport = child.bounds.size;
      stack.push((
        child,
        origin,
        current_viewport,
        false,
        has_fixed_cb_ancestor,
      ));
      current = child;
    } else {
      break;
    }
  }

  let mut out = Vec::new();
  for (node, origin, viewport, treat_as_root, has_fixed_cb_ancestor) in stack.into_iter().rev() {
    if let Some(state) = ScrollChainState::from_fragment(
      node,
      origin,
      viewport,
      root_viewport,
      treat_as_root,
      has_fixed_cb_ancestor,
    ) {
      out.push(state);
    }
  }

  out
}

pub fn build_scroll_chain<'a>(
  root: &'a FragmentNode,
  viewport: Size,
  path: &[usize],
) -> Vec<ScrollChainState<'a>> {
  build_scroll_chain_with_root_mode(root, viewport, path, true)
}

#[derive(Debug, Clone)]
pub struct ScrollChainResult {
  pub remaining: Point,
  pub overscroll: Vec<Point>,
}

fn apply_scroll_to_state(
  state: &mut ScrollChainState,
  delta: Point,
  options: ScrollOptions,
) -> (Point, Point) {
  let target_x = state.scroll.x + delta.x;
  let target_y = state.scroll.y + delta.y;

  let clamp_axis = |value: f32, min: f32, max: f32| {
    if value.is_finite() && min.is_finite() && max.is_finite() && min <= max {
      value.clamp(min, max)
    } else {
      value
    }
  };

  let clamped_x = clamp_axis(target_x, state.bounds.min_x, state.bounds.max_x);
  let clamped_y = clamp_axis(target_y, state.bounds.min_y, state.bounds.max_y);

  state.scroll = Point::new(clamped_x, clamped_y);

  let overshoot_x = target_x - clamped_x;
  let overshoot_y = target_y - clamped_y;

  let propagate_x = matches!(options.source, ScrollSource::User)
    && matches!(state.overscroll_behavior_x, OverscrollBehavior::Auto);
  let propagate_y = matches!(options.source, ScrollSource::User)
    && matches!(state.overscroll_behavior_y, OverscrollBehavior::Auto);

  let overscroll_x = if options.simulate_overscroll
    && !matches!(state.overscroll_behavior_x, OverscrollBehavior::None)
  {
    overshoot_x
  } else {
    0.0
  };

  let overscroll_y = if options.simulate_overscroll
    && !matches!(state.overscroll_behavior_y, OverscrollBehavior::None)
  {
    overshoot_y
  } else {
    0.0
  };

  (
    Point::new(
      if propagate_x { overshoot_x } else { 0.0 },
      if propagate_y { overshoot_y } else { 0.0 },
    ),
    Point::new(overscroll_x, overscroll_y),
  )
}

pub fn apply_scroll_chain(
  states: &mut [ScrollChainState],
  delta: Point,
  options: ScrollOptions,
) -> ScrollChainResult {
  // `ScrollChainResult::overscroll` is only used when a caller explicitly requests overscroll
  // simulation. Avoid an allocation for the common case (`simulate_overscroll=false`).
  let mut overscroll = if options.simulate_overscroll {
    Vec::with_capacity(states.len())
  } else {
    Vec::new()
  };
  let mut remaining = delta;

  for state in states.iter_mut() {
    let (leftover, over) = apply_scroll_to_state(state, remaining, options);
    remaining = leftover;

    if options.apply_snap {
      if let Some(snap) = state.snap.as_ref() {
        if let Some(style) = snap.container.style.as_ref() {
          state.scroll = apply_scroll_snap_for_container(
            snap.container,
            style,
            state.viewport,
            state.scroll,
            snap.origin,
            state.bounds,
          );
        }
      }
    }

    state.scroll = state.bounds.clamp(state.scroll);
    if options.simulate_overscroll {
      overscroll.push(over);
    }
  }

  ScrollChainResult {
    remaining,
    overscroll,
  }
}

fn apply_scroll_snap_for_container(
  container: &FragmentNode,
  style: &ComputedStyle,
  viewport: Size,
  scroll: Point,
  container_origin: Point,
  scroll_bounds: ScrollBounds,
) -> Point {
  if style.scroll_snap_type.axis == ScrollSnapAxis::None {
    return scroll_bounds.clamp(scroll);
  }

  let inline_vertical = is_vertical_writing_mode(style.writing_mode);
  let (snap_x, snap_y) = snap_axis_flags(style.scroll_snap_type.axis, inline_vertical);
  if !snap_x && !snap_y {
    return scroll;
  }

  let padding_x = (
    sanitize_scroll_padding(resolve_snap_length(
      style.scroll_padding_left,
      viewport.width,
    )),
    sanitize_scroll_padding(resolve_snap_length(
      style.scroll_padding_right,
      viewport.width,
    )),
  );
  let padding_y = (
    sanitize_scroll_padding(resolve_snap_length(
      style.scroll_padding_top,
      viewport.height,
    )),
    sanitize_scroll_padding(resolve_snap_length(
      style.scroll_padding_bottom,
      viewport.height,
    )),
  );
  let mut targets_x = Vec::new();
  let mut targets_y = Vec::new();
  let mut snap_bounds = SnapBounds::default();
  let container_offset = Point::new(-container_origin.x, -container_origin.y);
  collect_snap_targets(
    container,
    container_offset,
    inline_vertical,
    snap_x,
    snap_y,
    viewport,
    padding_x,
    padding_y,
    &mut snap_bounds,
    &mut targets_x,
    &mut targets_y,
  );

  targets_x.retain(|(p, _)| p.is_finite());
  targets_y.retain(|(p, _)| p.is_finite());

  if let Some(max_target_x) = targets_x
    .iter()
    .map(|(p, _)| *p)
    .max_by(|a, b| a.total_cmp(b))
  {
    snap_bounds.max_x = snap_bounds.max_x.max(max_target_x + viewport.width);
  }
  if let Some(max_target_y) = targets_y
    .iter()
    .map(|(p, _)| *p)
    .max_by(|a, b| a.total_cmp(b))
  {
    snap_bounds.max_y = snap_bounds.max_y.max(max_target_y + viewport.height);
  }

  let min_target_x = targets_x
    .iter()
    .map(|(p, _)| *p)
    .min_by(|a, b| a.total_cmp(b))
    .unwrap_or(0.0);
  let min_target_y = targets_y
    .iter()
    .map(|(p, _)| *p)
    .min_by(|a, b| a.total_cmp(b))
    .unwrap_or(0.0);
  if min_target_x > 0.0 {
    for (p, _) in &mut targets_x {
      *p -= min_target_x;
    }
    snap_bounds.max_x = (snap_bounds.max_x - min_target_x).max(0.0);
  }
  if min_target_y > 0.0 {
    for (p, _) in &mut targets_y {
      *p -= min_target_y;
    }
    snap_bounds.max_y = (snap_bounds.max_y - min_target_y).max(0.0);
  }

  let container_rect = Rect::from_xywh(
    container.bounds.x() + container_offset.x,
    container.bounds.y() + container_offset.y,
    container.bounds.width(),
    container.bounds.height(),
  );
  snap_bounds.update(container_rect);

  let max_scroll_x = (snap_bounds.max_x - viewport.width).max(0.0);
  let max_scroll_y = (snap_bounds.max_y - viewport.height).max(0.0);
  let strictness = style.scroll_snap_type.strictness;
  let shift_x = if min_target_x > 0.0 {
    min_target_x
  } else {
    0.0
  };
  let shift_y = if min_target_y > 0.0 {
    min_target_y
  } else {
    0.0
  };

  let snapped_x = if snap_x {
    pick_snap_target(
      scroll.x - shift_x,
      max_scroll_x,
      strictness,
      viewport.width * 0.5,
      &targets_x,
    ) + shift_x
  } else {
    scroll.x
  };
  let snapped_y = if snap_y {
    pick_snap_target(
      scroll.y - shift_y,
      max_scroll_y,
      strictness,
      viewport.height * 0.5,
      &targets_y,
    ) + shift_y
  } else {
    scroll.y
  };

  scroll_bounds.clamp(Point::new(snapped_x, snapped_y))
}

#[derive(Default, Debug)]
pub struct SnapBounds {
  pub max_x: f32,
  pub max_y: f32,
}

impl SnapBounds {
  pub fn update(&mut self, rect: Rect) {
    let max_x = rect.max_x();
    if max_x.is_finite() {
      self.max_x = self.max_x.max(max_x);
    }
    let max_y = rect.max_y();
    if max_y.is_finite() {
      self.max_y = self.max_y.max(max_y);
    }
  }
}

pub(crate) fn collect_snap_targets(
  node: &FragmentNode,
  offset: Point,
  inline_vertical: bool,
  snap_x: bool,
  snap_y: bool,
  viewport: Size,
  padding_x: (f32, f32),
  padding_y: (f32, f32),
  bounds: &mut SnapBounds,
  targets_x: &mut Vec<(f32, ScrollSnapStop)>,
  targets_y: &mut Vec<(f32, ScrollSnapStop)>,
) {
  let abs_bounds = Rect::from_xywh(
    node.bounds.x() + offset.x,
    node.bounds.y() + offset.y,
    node.bounds.width(),
    node.bounds.height(),
  );
  bounds.update(abs_bounds);

  if let Some(style) = node.style.as_ref() {
    if snap_x {
      let margin_start = sanitize_snap_length(resolve_snap_length(
        style.scroll_margin_left,
        viewport.width,
      ));
      let margin_end = sanitize_snap_length(resolve_snap_length(
        style.scroll_margin_right,
        viewport.width,
      ));
      let (padding_start, padding_end) = padding_x;
      let align_x = if inline_vertical {
        style.scroll_snap_align.block
      } else {
        style.scroll_snap_align.inline
      };
      let axis_positive = if inline_vertical {
        // Physical x maps to the block axis in vertical writing modes.
        !matches!(
          style.writing_mode,
          WritingMode::VerticalRl | WritingMode::SidewaysRl
        )
      } else {
        // Physical x maps to the inline axis in horizontal writing modes.
        matches!(style.direction, Direction::Ltr)
      };
      if let Some(pos) = snap_position(
        align_x,
        abs_bounds.x(),
        abs_bounds.max_x(),
        viewport.width,
        padding_start,
        padding_end,
        margin_start,
        margin_end,
        axis_positive,
      ) {
        targets_x.push((pos, style.scroll_snap_stop));
      }
    }
    if snap_y {
      let margin_start = sanitize_snap_length(resolve_snap_length(
        style.scroll_margin_top,
        viewport.height,
      ));
      let margin_end = sanitize_snap_length(resolve_snap_length(
        style.scroll_margin_bottom,
        viewport.height,
      ));
      let (padding_start, padding_end) = padding_y;
      let align_y = if inline_vertical {
        style.scroll_snap_align.inline
      } else {
        style.scroll_snap_align.block
      };
      let axis_positive = if inline_vertical {
        // Physical y maps to the inline axis in vertical writing modes.
        matches!(style.direction, Direction::Ltr)
      } else {
        // Physical y maps to the block axis in horizontal writing modes.
        true
      };
      if let Some(pos) = snap_position(
        align_y,
        abs_bounds.y(),
        abs_bounds.max_y(),
        viewport.height,
        padding_start,
        padding_end,
        margin_start,
        margin_end,
        axis_positive,
      ) {
        targets_y.push((pos, style.scroll_snap_stop));
      }
    }
  }

  let child_offset = Point::new(abs_bounds.x(), abs_bounds.y());
  for child in node.children.iter() {
    collect_snap_targets(
      child,
      child_offset,
      inline_vertical,
      snap_x,
      snap_y,
      viewport,
      padding_x,
      padding_y,
      bounds,
      targets_x,
      targets_y,
    );
  }
}

pub(crate) fn find_snap_container<'a>(
  node: &'a FragmentNode,
  origin: Point,
) -> Option<(&'a FragmentNode, &'a ComputedStyle, Point)> {
  if let Some(style) = node.style.as_ref() {
    if style.scroll_snap_type.axis != ScrollSnapAxis::None {
      return Some((node, style, origin));
    }
  }

  for child in node.children.iter() {
    let child_origin = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
    if let Some(found) = find_snap_container(child, child_origin) {
      return Some(found);
    }
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::{Point, Rect, Size};
  use crate::style::types::BorderStyle;
  use crate::tree::fragment_tree::FragmentContent;
  use crate::tree::fragment_tree::ScrollbarReservation;
  use std::sync::Arc;

  mod effective_scroll_state_test;
  mod offset_translates_promoted_fragments_test;
  mod overflow_clipping_test;
  mod scroll_anchoring_depends_on_scroll_offset_test;
  mod scroll_anchoring_missing_anchor_test;
  mod scroll_anchoring_scroll_padding_test;
  mod scroll_anchoring_writing_mode_test;
  mod scroll_blit_supported_test;

  fn container_style(axis: ScrollSnapAxis, strictness: ScrollSnapStrictness) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.scroll_snap_type.axis = axis;
    style.scroll_snap_type.strictness = strictness;
    Arc::new(style)
  }

  fn target_style(inline: ScrollSnapAlign, block: ScrollSnapAlign) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.scroll_snap_align.inline = inline;
    style.scroll_snap_align.block = block;
    Arc::new(style)
  }

  #[test]
  fn scrollport_rect_for_fragment_subtracts_borders_and_scrollbar_reservation() {
    let mut node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 100.0), vec![]);
    node.scrollbar_reservation = ScrollbarReservation {
      left: 10.0,
      right: 5.0,
      top: 3.0,
      bottom: 7.0,
    };

    let mut style = ComputedStyle::default();
    style.border_left_style = BorderStyle::Solid;
    style.border_right_style = BorderStyle::Solid;
    style.border_top_style = BorderStyle::Solid;
    style.border_bottom_style = BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(4.0);
    style.border_top_width = Length::px(6.0);
    style.border_bottom_width = Length::px(8.0);

    let rect = scrollport_rect_for_fragment(&node, &style);
    assert_eq!(rect, Rect::from_xywh(12.0, 9.0, 179.0, 76.0));
  }

  #[test]
  fn scrollport_rect_for_fragment_clamps_to_non_negative_sizes() {
    let mut node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    node.scrollbar_reservation = ScrollbarReservation {
      left: 5.0,
      right: 5.0,
      top: 5.0,
      bottom: 5.0,
    };

    let mut style = ComputedStyle::default();
    style.border_left_style = BorderStyle::Solid;
    style.border_right_style = BorderStyle::Solid;
    style.border_top_style = BorderStyle::Solid;
    style.border_bottom_style = BorderStyle::Solid;
    style.border_left_width = Length::px(8.0);
    style.border_right_width = Length::px(8.0);
    style.border_top_width = Length::px(8.0);
    style.border_bottom_width = Length::px(8.0);

    let rect = scrollport_rect_for_fragment(&node, &style);
    assert_eq!(rect, Rect::from_xywh(13.0, 13.0, 0.0, 0.0));
  }

  #[test]
  fn merge_containers_combines_flags() {
    let first = ScrollSnapContainer {
      box_id: Some(42),
      viewport: Size::new(100.0, 80.0),
      strictness: ScrollSnapStrictness::Proximity,
      behavior: ScrollBehavior::Auto,
      snap_x: true,
      snap_y: false,
      axis_is_inline_for_x: true,
      axis_is_inline_for_y: false,
      padding_x: (0.0, 2.0),
      padding_y: (1.0, 0.0),
      scroll_bounds: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      targets_x: vec![ScrollSnapTarget {
        box_id: None,
        position: 1.0,
        stop: ScrollSnapStop::Normal,
      }],
      targets_y: vec![],
      uses_viewport_scroll: false,
    };

    let second = ScrollSnapContainer {
      box_id: Some(42),
      viewport: Size::new(120.0, 100.0),
      strictness: ScrollSnapStrictness::Mandatory,
      behavior: ScrollBehavior::Smooth,
      snap_x: false,
      snap_y: true,
      axis_is_inline_for_x: true,
      axis_is_inline_for_y: false,
      padding_x: (5.0, 1.0),
      padding_y: (0.5, 4.0),
      scroll_bounds: Rect::from_xywh(5.0, 5.0, 10.0, 10.0),
      targets_x: vec![],
      targets_y: vec![ScrollSnapTarget {
        box_id: None,
        position: 2.0,
        stop: ScrollSnapStop::Always,
      }],
      uses_viewport_scroll: false,
    };

    let merged = merge_containers(vec![first, second]);
    assert_eq!(merged.len(), 1);
    let container = &merged[0];
    assert_eq!(container.box_id, Some(42));
    assert_eq!(container.strictness, ScrollSnapStrictness::Mandatory);
    assert_eq!(container.behavior, ScrollBehavior::Smooth);
    assert!(container.snap_x);
    assert!(container.snap_y);
    assert_eq!(container.viewport, Size::new(120.0, 100.0));
    assert_eq!(container.padding_x, (5.0, 2.0));
    assert_eq!(container.padding_y, (1.0, 4.0));
    assert_eq!(
      container.scroll_bounds,
      Rect::from_xywh(0.0, 0.0, 15.0, 15.0)
    );
    assert_eq!(container.targets_x.len(), 1);
    assert_eq!(container.targets_y.len(), 1);
  }

  #[test]
  fn build_scroll_metadata_does_not_promote_element_scrollers_to_viewport() {
    let root_style = Arc::new(ComputedStyle::default());
    let body_style = Arc::new(ComputedStyle::default());

    let mut first_style = ComputedStyle::default();
    first_style.scroll_snap_type.axis = ScrollSnapAxis::X;
    first_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    first_style.scroll_behavior = ScrollBehavior::Smooth;
    let first_style = Arc::new(first_style);

    let mut second_style = ComputedStyle::default();
    second_style.scroll_snap_type.axis = ScrollSnapAxis::X;
    second_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    second_style.scroll_behavior = ScrollBehavior::Auto;
    let second_style = Arc::new(second_style);

    let mut first_container =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![], first_style);
    if let FragmentContent::Block { box_id } = &mut first_container.content {
      *box_id = Some(10);
    }

    let mut second_container = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![],
      second_style,
    );
    if let FragmentContent::Block { box_id } = &mut second_container.content {
      *box_id = Some(11);
    }

    let root_first = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
        vec![first_container],
        body_style.clone(),
      )],
      root_style.clone(),
    );

    let root_second = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 120.0, 100.0, 100.0),
      vec![FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
        vec![second_container],
        body_style,
      )],
      root_style,
    );

    let mut tree = FragmentTree::with_viewport(root_first, Size::new(100.0, 100.0));
    tree.additional_fragments.push(root_second);

    let metadata = build_scroll_metadata(&mut tree);
    assert_eq!(metadata.containers.len(), 2);
    assert!(
      metadata.containers.iter().all(|c| !c.uses_viewport_scroll),
      "only the root scroll snap container should map to viewport scroll"
    );
  }

  #[test]
  fn both_axes_snap_to_start() {
    let mut container_style = ComputedStyle::default();
    container_style.scroll_snap_type.axis = ScrollSnapAxis::Both;
    container_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    let container_style = Arc::new(container_style);

    let mut child_style = ComputedStyle::default();
    child_style.scroll_snap_align.inline = ScrollSnapAlign::Start;
    child_style.scroll_snap_align.block = ScrollSnapAlign::Start;
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(120.0, 150.0, 50.0, 50.0),
      vec![],
      child_style,
    );

    let container = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 300.0, 300.0),
      vec![child],
      container_style,
    );
    let mut tree = FragmentTree::with_viewport(container, Size::new(100.0, 100.0));

    let snapped = apply_scroll_snap(
      &mut tree,
      &ScrollState::with_viewport(Point::new(90.0, 160.0)),
    )
    .state
    .viewport;
    assert!((snapped.x - 120.0).abs() < 0.1);
    assert!((snapped.y - 150.0).abs() < 0.1);
  }

  #[test]
  fn apply_scroll_snap_from_metadata_matches_apply_scroll_snap() {
    let mut container_style = ComputedStyle::default();
    container_style.scroll_snap_type.axis = ScrollSnapAxis::Both;
    container_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    let container_style = Arc::new(container_style);

    let mut child_style = ComputedStyle::default();
    child_style.scroll_snap_align.inline = ScrollSnapAlign::Start;
    child_style.scroll_snap_align.block = ScrollSnapAlign::Start;
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(120.0, 150.0, 50.0, 50.0),
      vec![],
      child_style,
    );

    let container = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 300.0, 300.0),
      vec![child],
      container_style,
    );
    let mut tree = FragmentTree::with_viewport(container, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let metadata = tree
      .scroll_metadata
      .as_ref()
      .expect("scroll metadata computed");

    let state = ScrollState::with_viewport(Point::new(90.0, 160.0));

    let mut tree_expected = tree.clone();
    let expected = apply_scroll_snap(&mut tree_expected, &state);
    let actual = apply_scroll_snap_from_metadata(metadata, &state);

    assert_eq!(actual, expected);
  }

  #[test]
  fn nested_containers_snap_independently() {
    let outer_style = container_style(ScrollSnapAxis::Y, ScrollSnapStrictness::Mandatory);
    let inner_style = container_style(ScrollSnapAxis::X, ScrollSnapStrictness::Mandatory);

    let mut inner_child = FragmentNode::new_block_styled(
      Rect::from_xywh(50.0, 0.0, 120.0, 80.0),
      vec![FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 60.0, 80.0),
        vec![],
        target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
      )],
      inner_style.clone(),
    );
    if let FragmentContent::Block { box_id } = &mut inner_child.content {
      *box_id = Some(2);
    }

    let mut outer = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 200.0),
      vec![
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
          vec![],
          target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
        ),
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 120.0, 100.0, 100.0),
          vec![inner_child],
          target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
        ),
      ],
      outer_style,
    );

    if let FragmentContent::Block { box_id } = &mut outer.content {
      *box_id = Some(1);
    }
    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));

    let mut state = ScrollState::with_viewport(Point::new(0.0, 130.0));
    state.elements.insert(2, Point::new(70.0, 0.0));

    let snapped = apply_scroll_snap(&mut tree, &state);
    let outer_offset = snapped.state.viewport;
    assert!((outer_offset.y - 120.0).abs() < 0.1);

    let inner_offset = snapped.state.elements.get(&2).copied().unwrap();
    assert!(inner_offset.x.abs() < 0.1);
  }

  #[test]
  fn apply_scroll_snap_from_metadata_matches_apply_scroll_snap_nested_containers() {
    let outer_style = container_style(ScrollSnapAxis::Y, ScrollSnapStrictness::Mandatory);
    let inner_style = container_style(ScrollSnapAxis::X, ScrollSnapStrictness::Mandatory);

    let mut inner_child = FragmentNode::new_block_styled(
      Rect::from_xywh(50.0, 0.0, 120.0, 80.0),
      vec![FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 60.0, 80.0),
        vec![],
        target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
      )],
      inner_style,
    );
    if let FragmentContent::Block { box_id } = &mut inner_child.content {
      *box_id = Some(2);
    }

    let mut outer = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 200.0),
      vec![
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
          vec![],
          target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
        ),
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 120.0, 100.0, 100.0),
          vec![inner_child],
          target_style(ScrollSnapAlign::Start, ScrollSnapAlign::Start),
        ),
      ],
      outer_style,
    );
    if let FragmentContent::Block { box_id } = &mut outer.content {
      *box_id = Some(1);
    }

    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let metadata = tree
      .scroll_metadata
      .as_ref()
      .expect("ensure_scroll_metadata should populate scroll_metadata");

    let mut state = ScrollState::with_viewport(Point::new(0.0, 130.0));
    state.elements.insert(2, Point::new(70.0, 0.0));

    let expected = {
      let mut tree_clone = tree.clone();
      apply_scroll_snap(&mut tree_clone, &state).state
    };
    let actual = apply_scroll_snap_from_metadata(metadata, &state).state;

    assert_eq!(actual, expected);
  }

  #[test]
  fn rtl_inline_start_uses_physical_right() {
    let mut container_style = ComputedStyle::default();
    container_style.scroll_snap_type.axis = ScrollSnapAxis::X;
    container_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    container_style.direction = Direction::Rtl;
    let container_style = Arc::new(container_style);

    let mut target_style = ComputedStyle::default();
    target_style.scroll_snap_align.inline = ScrollSnapAlign::Start;
    let target_style = Arc::new(target_style);

    let target =
      FragmentNode::new_block_styled(Rect::from_xywh(80.0, 0.0, 80.0, 50.0), vec![], target_style);

    let container = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 80.0),
      vec![target],
      container_style,
    );
    let mut tree = FragmentTree::with_viewport(container, Size::new(100.0, 80.0));

    let state = ScrollState::with_viewport(Point::new(70.0, 0.0));
    let snapped = apply_scroll_snap(&mut tree, &state);
    assert!((snapped.state.viewport.x - 60.0).abs() < 0.1);
  }

  fn make_vertical_nested(inner_behavior: OverscrollBehavior) -> FragmentNode {
    let mut inner_style = ComputedStyle::default();
    inner_style.overflow_y = Overflow::Scroll;
    inner_style.overscroll_behavior_y = inner_behavior;

    let inner_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![inner_child],
      Arc::new(inner_style),
    );

    let mut outer_style = ComputedStyle::default();
    outer_style.overflow_y = Overflow::Scroll;

    let trailing = FragmentNode::new_block(Rect::from_xywh(0.0, 250.0, 100.0, 150.0), vec![]);
    FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![inner, trailing],
      Arc::new(outer_style),
    )
  }

  #[test]
  fn overscroll_contain_blocks_chaining() {
    let outer = make_vertical_nested(OverscrollBehavior::Contain);
    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let mut chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 400.0), ScrollOptions::default());

    assert_eq!(chain.len(), 2, "inner and outer should both participate");
    assert!(
      (chain[0].scroll.y - 200.0).abs() < 1e-3,
      "inner should clamp to max"
    );
    assert!(
      chain[1].scroll.y.abs() < 1e-3,
      "outer should not chain past contain"
    );
    assert!(
      result.remaining.y.abs() < 1e-3,
      "no scroll should leak past outer"
    );
  }

  #[test]
  fn overscroll_auto_chains_to_parent() {
    let outer = make_vertical_nested(OverscrollBehavior::Auto);
    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let mut chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 400.0), ScrollOptions::default());

    assert!(
      (chain[0].scroll.y - 200.0).abs() < 1e-3,
      "inner should still clamp"
    );
    assert!(
      chain[1].scroll.y > 0.0,
      "outer should receive chained delta"
    );
    assert!(
      result.remaining.y.abs() < 1e-3,
      "all scroll should be consumed"
    );
  }

  #[test]
  fn overscroll_none_suppresses_indicator() {
    let outer = make_vertical_nested(OverscrollBehavior::None);
    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let mut chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
    let mut options = ScrollOptions::default();
    options.simulate_overscroll = true;
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 300.0), options);

    assert!(
      result.overscroll[0].y.abs() < 1e-3,
      "overscroll glow should be suppressed"
    );
    assert!(
      chain[1].scroll.y.abs() < 1e-3,
      "outer should not chain when none is set"
    );
  }

  #[test]
  fn horizontal_contain_respects_rtl_and_vertical_writing() {
    let mut inner_style = ComputedStyle::default();
    inner_style.overflow_x = Overflow::Scroll;
    inner_style.overscroll_behavior_x = OverscrollBehavior::Contain;
    inner_style.writing_mode = WritingMode::VerticalRl;

    let inner_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 300.0, 100.0), vec![]);
    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![inner_child],
      Arc::new(inner_style),
    );

    let mut outer_style = ComputedStyle::default();
    outer_style.overflow_x = Overflow::Scroll;
    outer_style.direction = Direction::Rtl;

    let sibling = FragmentNode::new_block(Rect::from_xywh(220.0, 0.0, 200.0, 100.0), vec![]);
    let outer = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![inner, sibling],
      Arc::new(outer_style),
    );

    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let mut chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
    let result = apply_scroll_chain(&mut chain, Point::new(400.0, 0.0), ScrollOptions::default());

    assert!(
      (chain[0].scroll.x - 200.0).abs() < 1e-3,
      "inner should clamp on x"
    );
    assert!(
      chain[1].scroll.x.abs() < 1e-3,
      "outer should not receive chained x scroll"
    );
    assert!(
      result.remaining.x.abs() < 1e-3,
      "scroll should stop at contained edge"
    );
  }

  #[test]
  fn chained_scroll_still_snaps_outer_container() {
    let mut inner_style = ComputedStyle::default();
    inner_style.overflow_y = Overflow::Scroll;
    inner_style.scroll_snap_align.block = ScrollSnapAlign::Start;
    inner_style.scroll_snap_align.inline = ScrollSnapAlign::Start;

    let inner_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 120.0), vec![]);
    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![inner_child],
      Arc::new(inner_style),
    );

    let mut snap_child_style = ComputedStyle::default();
    snap_child_style.scroll_snap_align.block = ScrollSnapAlign::Start;
    snap_child_style.scroll_snap_align.inline = ScrollSnapAlign::Start;

    let second = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 200.0, 100.0, 200.0),
      vec![],
      Arc::new(snap_child_style.clone()),
    );

    let mut outer_style = ComputedStyle::default();
    outer_style.overflow_y = Overflow::Scroll;
    outer_style.scroll_snap_type.axis = ScrollSnapAxis::Y;
    outer_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;

    let outer = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
          vec![FragmentNode::new_block(
            Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
            vec![],
          )],
          Arc::new(ComputedStyle {
            scroll_snap_align: snap_child_style.scroll_snap_align,
            overflow_y: Overflow::Scroll,
            ..ComputedStyle::default()
          }),
        ),
        inner,
        second,
      ],
      Arc::new(outer_style),
    );

    let mut tree = FragmentTree::with_viewport(outer, Size::new(100.0, 100.0));
    tree.ensure_scroll_metadata();
    let mut chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[1]);
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 180.0), ScrollOptions::default());

    assert!(result.remaining.y.abs() < 1e-3);
    assert!(
      (chain[1].scroll.y - 200.0).abs() < 1e-2,
      "outer should snap to next item"
    );
  }

  #[test]
  fn apply_scroll_chain_can_defer_snapping_with_apply_snap_false() {
    let mut target_style = ComputedStyle::default();
    target_style.scroll_snap_align.block = ScrollSnapAlign::Start;
    target_style.scroll_snap_align.inline = ScrollSnapAlign::Start;
    let target_style = Arc::new(target_style);

    let first = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![],
      target_style.clone(),
    );
    let second = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 200.0, 100.0, 100.0),
      vec![],
      target_style,
    );

    let mut container_style = ComputedStyle::default();
    container_style.overflow_y = Overflow::Scroll;
    container_style.scroll_snap_type.axis = ScrollSnapAxis::Y;
    container_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
    let container_style = Arc::new(container_style);

    let container = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![first, second],
      container_style,
    );

    // With snapping enabled, the delta lands near the second snap point and should snap to it.
    let mut chain = build_scroll_chain(&container, Size::new(100.0, 100.0), &[0]);
    assert_eq!(chain.len(), 1);
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 180.0), ScrollOptions::default());
    assert!(result.remaining.y.abs() < 1e-3);
    assert!((chain[0].scroll.y - 200.0).abs() < 1e-2);

    // With snapping disabled, we should preserve the raw accumulated scroll offset.
    let mut chain = build_scroll_chain(&container, Size::new(100.0, 100.0), &[0]);
    let mut options = ScrollOptions::default();
    options.apply_snap = false;
    let result = apply_scroll_chain(&mut chain, Point::new(0.0, 180.0), options);
    assert!(result.remaining.y.abs() < 1e-3);
    assert!((chain[0].scroll.y - 180.0).abs() < 1e-2);
  }
}
