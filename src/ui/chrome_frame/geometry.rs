use crate::geometry::{Point, Rect};
use crate::dom::DomNode;
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxNode;
use crate::PreparedDocument;

fn preorder_id_for_element_id(dom: &DomNode, element_id: &str) -> Option<usize> {
  // Match `crate::dom::enumerate_dom_ids` preorder traversal and `DomIndex` semantics:
  // - IDs are assigned to every node in pre-order (including `<template>` contents)
  // - `id` attribute lookup ignores inert `<template>` subtrees (matching `getElementById`)
  //
  // We intentionally traverse `children` (not `traversal_children`) so template contents still count
  // towards stable preorder ids.
  let mut next_id: usize = 0;
  let mut stack: Vec<(&DomNode, bool)> = vec![(dom, false)];
  while let Some((node, in_template_contents)) = stack.pop() {
    next_id += 1;

    if !in_template_contents {
      if let Some(id) = node.get_attribute_ref("id") {
        if id == element_id {
          return Some(next_id);
        }
      }
    }

    let child_in_template_contents = in_template_contents || node.is_template_element();
    for child in node.children.iter().rev() {
      stack.push((child, child_in_template_contents));
    }
  }

  None
}

pub fn element_border_rect_by_id(prepared: &PreparedDocument, element_id: &str) -> Option<Rect> {
  let scroll_state = prepared.default_scroll_state();
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}

pub fn element_border_rect_by_id_with_scroll_state(
  prepared: &PreparedDocument,
  element_id: &str,
  scroll_state: &ScrollState,
) -> Option<Rect> {
  let node_id = preorder_id_for_element_id(prepared.dom(), element_id)?;

  // Convert the fragment tree into paint-time geometry coordinates (element scroll offsets +
  // sticky adjustments). The returned tree is still in page coordinates; convert it to
  // viewport-local space at the end by subtracting the viewport scroll offset.
  let geometry_tree = prepared.fragment_tree_for_geometry_fast(scroll_state);

  let mut out: Option<Rect> = None;
  // Match `BoxTree::assign_implicit_anchor_box_ids` traversal order.
  let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree().root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(node_id) {
      if let Some(bounds) = crate::interaction::fragment_geometry::absolute_bounds_for_box_id(
        &geometry_tree,
        node.id,
      ) {
        out = Some(match out {
          Some(existing) => existing.union(bounds),
          None => bounds,
        });
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let rect = out?;
  let viewport_scroll = scroll_state.viewport;
  Some(rect.translate(Point::new(-viewport_scroll.x, -viewport_scroll.y)))
}

pub fn element_border_rect_by_id_with_viewport_scroll(
  prepared: &PreparedDocument,
  element_id: &str,
  viewport_scroll: Point,
) -> Option<Rect> {
  let scroll_state = ScrollState::with_viewport(viewport_scroll);
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}

/// Converts a chrome layout rect (CSS pixels) into physical pixel coordinates.
///
/// The returned tuple is `(x, y, w, h)` in **physical pixels** and is suitable for allocating and
/// positioning textures.
///
/// # Rounding rules (to avoid seams)
///
/// We scale the rect's edges by `dpr` and then:
/// - **floor** the minimum edges (origin)
/// - **ceil** the maximum edges (origin + size)
///
/// This guarantees that two adjacent CSS rects remain edge-aligned after conversion, even when
/// their boundaries are fractional in physical pixels (avoiding 1px cracks from asymmetric
/// rounding).
///
/// # Robustness
///
/// Any negative or non-finite edge values (including `NaN`/`∞`, or negative `dpr`) are clamped to
/// `0`. Width/height are computed with `saturating_sub` so they never underflow.
pub fn rect_css_to_physical(rect_css: Rect, dpr: f32) -> (u32, u32, u32, u32) {
  if !dpr.is_finite() || dpr <= 0.0 {
    return (0, 0, 0, 0);
  }

  let x0 = clamp_physical_edge((rect_css.min_x() * dpr).floor());
  let y0 = clamp_physical_edge((rect_css.min_y() * dpr).floor());
  let x1 = clamp_physical_edge((rect_css.max_x() * dpr).ceil());
  let y1 = clamp_physical_edge((rect_css.max_y() * dpr).ceil());

  // `as` is safe here: we already clamped to finite, non-negative values.
  let x0 = x0 as u32;
  let y0 = y0 as u32;
  let x1 = x1 as u32;
  let y1 = y1 as u32;

  (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

fn clamp_physical_edge(v: f32) -> f32 {
  if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
  use super::rect_css_to_physical;
  use crate::geometry::Rect;

  #[test]
  fn rect_css_to_physical_dpr_1() {
    let rect = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    assert_eq!(rect_css_to_physical(rect, 1.0), (10, 20, 100, 50));
  }

  #[test]
  fn rect_css_to_physical_dpr_2() {
    let rect = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    assert_eq!(rect_css_to_physical(rect, 2.0), (20, 40, 200, 100));
  }

  #[test]
  fn rect_css_to_physical_fractional_floor_origin_ceil_max() {
    let rect = Rect::from_xywh(10.25, 20.25, 5.5, 6.5);
    assert_eq!(rect_css_to_physical(rect, 2.0), (20, 40, 12, 14));
  }

  #[test]
  fn rect_css_to_physical_clamps_negative_and_nan() {
    let rect_neg = Rect::from_xywh(-10.0, -5.0, 20.0, 10.0);
    assert_eq!(rect_css_to_physical(rect_neg, 1.0), (0, 0, 10, 5));

    let rect_nan = Rect::from_xywh(f32::NAN, 5.0, 10.0, 10.0);
    assert_eq!(rect_css_to_physical(rect_nan, 1.0), (0, 5, 0, 10));
  }

  #[test]
  fn rect_css_to_physical_width_height_do_not_underflow() {
    let rect = Rect::from_xywh(10.0, 20.0, -5.0, -6.0);
    assert_eq!(rect_css_to_physical(rect, 1.0), (10, 20, 0, 0));
  }
}
