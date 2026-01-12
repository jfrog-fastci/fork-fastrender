use crate::dom2;
use crate::geometry::{Point, Rect, Size};
use crate::scroll::ScrollState;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::ScrollbarReservation;

use super::{FastRender, PreparedDocument};

/// Scroll- and sticky-aware DOM2 geometry queries.
///
/// This context is intended for DOM geometry APIs that need to mirror how the renderer positions
/// fragments for a given scroll state (e.g. `IntersectionObserver` root rect calculations or
/// `ResizeObserver` box metrics).
///
/// Coordinates returned by `*_in_viewport` helpers are in CSS pixels, relative to the top-left of
/// the layout viewport (i.e. translated by `-scroll_state.viewport`).
pub struct Dom2GeometryContext<'a> {
  prepared: &'a PreparedDocument,
  dom_mapping: &'a dom2::RendererDomMapping,
  fragment_tree: crate::tree::fragment_tree::FragmentTree,
  viewport_scroll: Point,
  viewport_size: Size,
}

impl<'a> Dom2GeometryContext<'a> {
  pub fn new(
    renderer: &FastRender,
    prepared: &'a PreparedDocument,
    dom_mapping: &'a dom2::RendererDomMapping,
    scroll_state: ScrollState,
  ) -> Self {
    let mut fragment_tree = prepared.fragment_tree().clone();

    // Mirror the paint pipeline: apply scroll snap before sticky offsets so sticky positioning is
    // computed against the snapped scroll state.
    let scroll_result = crate::scroll::apply_scroll_snap(&mut fragment_tree, &scroll_state);
    let scroll_state = scroll_result.state;

    renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut fragment_tree, &scroll_state);
    crate::scroll::apply_scroll_offsets(&mut fragment_tree, &scroll_state);

    Self {
      prepared,
      dom_mapping,
      fragment_tree,
      viewport_scroll: scroll_state.viewport,
      // Use the visual viewport when resolving viewport-relative lengths, mirroring paint.
      viewport_size: prepared.visual_viewport(),
    }
  }

  fn styled_node_id_for_dom2_node(&self, node: dom2::NodeId) -> Option<usize> {
    self.dom_mapping.preorder_for_node_id(node)
  }

  fn boxes_for_styled_node_id(&self, styled_node_id: usize) -> Vec<&BoxNode> {
    let mut out = Vec::new();
    let mut stack: Vec<&BoxNode> = vec![&self.prepared.box_tree().root];

    while let Some(node) = stack.pop() {
      if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
        out.push(node);
      }

      // Mirror `BoxTree::assign_box_ids` traversal ordering.
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    out
  }

  fn translate_to_viewport(&self, rect: Rect) -> Rect {
    rect.translate(Point::new(-self.viewport_scroll.x, -self.viewport_scroll.y))
  }

  pub fn border_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) = crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      out = Some(match out {
        Some(existing) => existing.union(border_box),
        None => border_box,
      });
    }

    out
  }

  pub fn padding_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) = crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      let padding_box = padding_rect_for_border_rect(border_box, &box_node.style, self.viewport_size);
      out = Some(match out {
        Some(existing) => existing.union(padding_box),
        None => padding_box,
      });
    }

    out
  }

  pub fn content_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) = crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      let content_box =
        crate::interaction::content_rect_for_border_rect(border_box, &box_node.style, self.viewport_size);
      out = Some(match out {
        Some(existing) => existing.union(content_box),
        None => content_box,
      });
    }

    out
  }

  /// Returns the scrollport/client rect (padding box minus reserved scrollbar gutters) in viewport
  /// coordinates.
  ///
  /// This should be preferred over `padding_box_in_viewport` when computing observer roots and other
  /// "visible scrollport" geometry, because it accounts for classic (non-overlay) scrollbars / `scrollbar-gutter: stable`
  /// reservations captured during layout.
  pub fn scrollport_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let padding_box = self.padding_box_in_viewport(node)?;
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    // Prefer the principal box id (first in pre-order) when looking up scrollbar gutters.
    let mut reservation = ScrollbarReservation::default();
    for box_node in boxes {
      if let Some(found) =
        crate::interaction::scrollbar_reservation_for_box_id(&self.fragment_tree, box_node.id)
      {
        reservation = found;
        break;
      }
    }

    Some(crate::interaction::scrollport_rect_for_padding_rect(
      padding_box,
      reservation,
    ))
  }
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  let new_x = rect.x() + left;
  let new_y = rect.y() + top;
  let new_w = (rect.width() - left - right).max(0.0);
  let new_h = (rect.height() - top - bottom).max(0.0);
  Rect::from_xywh(new_x, new_y, new_w, new_h)
}

fn padding_rect_for_border_rect(border_rect: Rect, style: &ComputedStyle, viewport_size: Size) -> Rect {
  let font_size = style.font_size;
  let base = border_rect.width().max(0.0);
  let viewport = (viewport_size.width.is_finite() && viewport_size.height.is_finite())
    .then_some((viewport_size.width, viewport_size.height));

  let border_left = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_left_width(),
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let border_right = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_right_width(),
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let border_top = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_top_width(),
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let border_bottom = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_bottom_width(),
    font_size,
    style.root_font_size,
    base,
    viewport,
  );

  inset_rect(
    border_rect,
    border_left,
    border_top,
    border_right,
    border_bottom,
  )
}

