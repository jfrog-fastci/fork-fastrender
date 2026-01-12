use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
use crate::tree::fragment_tree::ScrollbarReservation;

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

pub fn scrollbar_reservation_for_box_id(
  tree: &FragmentTree,
  box_id: usize,
) -> Option<ScrollbarReservation> {
  struct Frame<'a> {
    node: &'a FragmentNode,
  }

  let mut found = false;
  let mut combined = ScrollbarReservation::default();

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in tree.additional_fragments.iter().rev() {
    stack.push(Frame { node: root });
  }
  stack.push(Frame { node: &tree.root });

  while let Some(frame) = stack.pop() {
    if frame.node.box_id() == Some(box_id) {
      found = true;
      let reservation = frame.node.scrollbar_reservation;
      combined.left = combined.left.max(reservation.left);
      combined.right = combined.right.max(reservation.right);
      combined.top = combined.top.max(reservation.top);
      combined.bottom = combined.bottom.max(reservation.bottom);
    }

    for child in frame.node.children.iter().rev() {
      stack.push(Frame { node: child });
    }
  }

  found.then_some(combined)
}

pub fn scrollport_rect_for_padding_rect(
  padding_rect: Rect,
  reservation: ScrollbarReservation,
) -> Rect {
  inset_rect(
    padding_rect,
    reservation.left,
    reservation.top,
    reservation.right,
    reservation.bottom,
  )
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  let new_x = rect.x() + left;
  let new_y = rect.y() + top;
  let new_w = (rect.width() - left - right).max(0.0);
  let new_h = (rect.height() - top - bottom).max(0.0);
  Rect::from_xywh(new_x, new_y, new_w, new_h)
}

/// Computes the padding box rect for a fragment border box using the computed style.
///
/// This mirrors the border inset logic in `paint::display_list_builder::background_rects` so
/// hit-testing and other interaction geometry can align with the actual painted geometry (including
/// UA default borders).
pub fn padding_rect_for_border_rect(
  border_rect: Rect,
  style: &ComputedStyle,
  viewport_size: Size,
) -> Rect {
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

/// Computes the content box rect for a fragment border box using the computed style.
///
/// This mirrors `paint::display_list_builder::background_rects` so hit-testing and tests can align
/// with the actual painted geometry (including UA default borders/padding).
pub fn content_rect_for_border_rect(
  border_rect: Rect,
  style: &ComputedStyle,
  viewport_size: Size,
) -> Rect {
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

#[cfg(test)]
mod tests {
  use super::absolute_bounds_for_box_id;
  use crate::geometry::Rect;
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

  #[test]
  fn absolute_bounds_for_box_id_accumulates_ancestor_offsets() {
    let target_box_id = 1;

    let target_fragment = FragmentNode::new(
      Rect::from_xywh(5.0, 6.0, 7.0, 8.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Canvas,
        box_id: Some(target_box_id),
      },
      vec![],
    );

    let parent = FragmentNode::new_block(
      Rect::from_xywh(10.0, 20.0, 100.0, 50.0),
      vec![target_fragment],
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![parent]);
    let tree = FragmentTree::new(root);

    let bounds =
      absolute_bounds_for_box_id(&tree, target_box_id).expect("expected box id to resolve");
    assert_eq!(bounds, Rect::from_xywh(15.0, 26.0, 7.0, 8.0));
  }

  #[test]
  fn absolute_bounds_for_box_id_searches_additional_fragments() {
    let target_box_id = 2;

    let target_fragment =
      FragmentNode::new_block_with_id(Rect::from_xywh(3.0, 4.0, 5.0, 6.0), target_box_id, vec![]);
    let additional_root = FragmentNode::new_block(
      Rect::from_xywh(100.0, 200.0, 300.0, 400.0),
      vec![target_fragment],
    );

    let mut tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
    ));
    tree.additional_fragments.push(additional_root);

    let bounds =
      absolute_bounds_for_box_id(&tree, target_box_id).expect("expected box id to resolve");
    assert_eq!(bounds, Rect::from_xywh(103.0, 204.0, 5.0, 6.0));
  }

  #[test]
  fn absolute_bounds_for_box_id_unions_multiple_fragments() {
    let target_box_id = 3;

    let fragment_a =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), target_box_id, vec![]);
    let fragment_b = FragmentNode::new_block_with_id(
      Rect::from_xywh(20.0, 5.0, 10.0, 10.0),
      target_box_id,
      vec![],
    );

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![fragment_a, fragment_b],
    );
    let tree = FragmentTree::new(root);

    let bounds =
      absolute_bounds_for_box_id(&tree, target_box_id).expect("expected box id to resolve");
    assert_eq!(bounds, Rect::from_xywh(0.0, 0.0, 30.0, 15.0));
  }
}
