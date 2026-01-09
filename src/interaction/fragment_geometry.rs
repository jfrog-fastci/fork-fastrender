use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;

pub fn absolute_bounds_for_box_id(tree: &FragmentTree, box_id: usize) -> Option<Rect> {
  struct Frame<'a> {
    node: &'a FragmentNode,
    parent_offset: Point,
  }

  let mut bounds: Option<Rect> = None;

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  stack.push(Frame {
    node: &tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = stack.pop() {
    let absolute_bounds = frame.node.bounds.translate(frame.parent_offset);
    if frame.node.box_id() == Some(box_id) {
      bounds = Some(match bounds {
        Some(existing) => existing.union(absolute_bounds),
        None => absolute_bounds,
      });
    }

    let child_parent_offset = absolute_bounds.origin;
    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  bounds
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  let new_x = rect.x() + left;
  let new_y = rect.y() + top;
  let new_w = (rect.width() - left - right).max(0.0);
  let new_h = (rect.height() - top - bottom).max(0.0);
  Rect::from_xywh(new_x, new_y, new_w, new_h)
}

/// Computes the content box rect for a fragment border box using the computed style.
///
/// This mirrors `paint::display_list_builder::background_rects` so hit-testing and tests can align
/// with the actual painted geometry (including UA default borders/padding).
pub fn content_rect_for_border_rect(border_rect: Rect, style: &ComputedStyle, viewport_size: Size) -> Rect {
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

  let padding_left = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_left,
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let padding_right = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_right,
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let padding_top = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_top,
    font_size,
    style.root_font_size,
    base,
    viewport,
  );
  let padding_bottom = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_bottom,
    font_size,
    style.root_font_size,
    base,
    viewport,
  );

  let padding_rect = inset_rect(
    border_rect,
    border_left,
    border_top,
    border_right,
    border_bottom,
  );
  inset_rect(
    padding_rect,
    padding_left,
    padding_top,
    padding_right,
    padding_bottom,
  )
}
