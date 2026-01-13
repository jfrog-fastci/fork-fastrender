use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree};
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
use crate::tree::fragment_tree::ScrollbarReservation;
use std::collections::HashMap;

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

/// Computes absolute bounds for each styled node id found in the rendered fragment tree.
///
/// The returned map is keyed by `styled_node_id` (DOM pre-order id) and the value is the union of
/// all fragment bounds produced for any box associated with that styled node.
///
/// Notes:
/// - This is **O(N)** over the box tree + fragment tree: it builds a `box_id → styled_node_id`
///   lookup in one pass over `BoxTree`, then unions fragment bounds in one pass over the fragment
///   roots (including additional fragments and nested running/footnote snapshots).
/// - Fragment bounds are translated into absolute coordinates by accumulating ancestor
///   `Rect::origin` offsets, matching [`absolute_bounds_for_box_id`].
/// - Generated pseudo-element boxes are **included** in the originating element's bounds. Pseudo
///   boxes share the originating element's `styled_node_id`, so their fragments naturally union
///   into the same entry.
pub fn absolute_bounds_by_styled_node_id(
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
) -> HashMap<usize, Rect> {
  let mut box_id_to_styled: Vec<Option<usize>> = vec![None];
  let mut box_stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = box_stack.pop() {
    let id = node.id;
    if id == 0 {
      continue;
    }
    if id >= box_id_to_styled.len() {
      box_id_to_styled.resize(id + 1, None);
    }
    box_id_to_styled[id] = node.styled_node_id;

    if let Some(body) = node.footnote_body.as_deref() {
      box_stack.push(body);
    }
    for child in node.children.iter().rev() {
      box_stack.push(child);
    }
  }

  struct Frame<'a> {
    node: &'a FragmentNode,
    parent_offset: Point,
  }

  let mut bounds_by_styled: HashMap<usize, Rect> = HashMap::new();
  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  stack.push(Frame {
    node: &fragment_tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = stack.pop() {
    let absolute_bounds = frame.node.bounds.translate(frame.parent_offset);

    if let Some(box_id) = frame.node.box_id() {
      if let Some(Some(styled_node_id)) = box_id_to_styled.get(box_id) {
        bounds_by_styled
          .entry(*styled_node_id)
          .and_modify(|existing| *existing = existing.union(absolute_bounds))
          .or_insert(absolute_bounds);
      }
    }

    let child_parent_offset = absolute_bounds.origin;

    match &frame.node.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        stack.push(Frame {
          node: snapshot.as_ref(),
          parent_offset: child_parent_offset,
        });
      }
      _ => {}
    }

    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  bounds_by_styled
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
  use super::{
    absolute_bounds_by_styled_node_id, absolute_bounds_for_box_id, scrollbar_reservation_for_box_id,
    scrollport_rect_for_padding_rect,
  };
  use crate::geometry::Rect;
  use crate::style::display::FormattingContextType;
  use crate::style::types::FootnotePolicy;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::{BoxNode, BoxTree, GeneratedPseudoElement};
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree, ScrollbarReservation};
  use std::sync::Arc;

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

  #[test]
  fn scrollbar_reservation_for_box_id_collects_fragment_reservation() {
    let target_box_id = 1;

    let mut target_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      target_box_id,
      vec![],
    );
    target_fragment.scrollbar_reservation = ScrollbarReservation {
      right: 10.0,
      bottom: 5.0,
      ..ScrollbarReservation::default()
    };

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![target_fragment],
    );
    let tree = FragmentTree::new(root);

    let reservation =
      scrollbar_reservation_for_box_id(&tree, target_box_id).expect("reservation");
    assert_eq!(
      reservation,
      ScrollbarReservation {
        right: 10.0,
        bottom: 5.0,
        ..ScrollbarReservation::default()
      }
    );
  }

  #[test]
  fn scrollbar_reservation_for_box_id_combines_multiple_fragments_conservatively() {
    let target_box_id = 1;

    let mut frag_a = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      target_box_id,
      vec![],
    );
    frag_a.scrollbar_reservation = ScrollbarReservation {
      right: 10.0,
      bottom: 5.0,
      ..ScrollbarReservation::default()
    };

    let mut frag_b = FragmentNode::new_block_with_id(
      Rect::from_xywh(20.0, 0.0, 10.0, 10.0),
      target_box_id,
      vec![],
    );
    frag_b.scrollbar_reservation = ScrollbarReservation {
      left: 4.0,
      right: 3.0,
      bottom: 12.0,
      ..ScrollbarReservation::default()
    };

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![frag_a, frag_b],
    );
    let tree = FragmentTree::new(root);

    let reservation =
      scrollbar_reservation_for_box_id(&tree, target_box_id).expect("reservation");
    assert_eq!(
      reservation,
      ScrollbarReservation {
        left: 4.0,
        right: 10.0,
        bottom: 12.0,
        ..ScrollbarReservation::default()
      }
    );
  }

  #[test]
  fn scrollbar_reservation_for_box_id_returns_none_when_missing() {
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![]);
    let tree = FragmentTree::new(root);

    assert_eq!(scrollbar_reservation_for_box_id(&tree, 42), None);
  }

  #[test]
  fn scrollport_rect_for_padding_rect_insets_by_reservation() {
    let reservation = ScrollbarReservation {
      right: 10.0,
      bottom: 5.0,
      ..ScrollbarReservation::default()
    };

    let padding_rect = Rect::from_xywh(0.0, 0.0, 100.0, 50.0);
    let scrollport = scrollport_rect_for_padding_rect(padding_rect, reservation);
    assert_eq!(scrollport, Rect::from_xywh(0.0, 0.0, 90.0, 45.0));
  }

  #[test]
  fn absolute_bounds_by_styled_node_id_unions_split_fragments_and_includes_pseudo_boxes() {
    let mut root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    root.styled_node_id = Some(1);

    let mut principal = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    principal.styled_node_id = Some(2);

    let mut pseudo = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    pseudo.styled_node_id = Some(2);
    pseudo.generated_pseudo = Some(GeneratedPseudoElement::Before);

    root.children = vec![principal, pseudo];
    let box_tree = BoxTree::new(root);
    let root_box_id = box_tree.root.id;
    let principal_box_id = box_tree.root.children[0].id;
    let pseudo_box_id = box_tree.root.children[1].id;

    let fragment_a = FragmentNode::new_block_with_id(
      Rect::from_xywh(5.0, 0.0, 10.0, 10.0),
      principal_box_id,
      vec![],
    );
    let fragment_b = FragmentNode::new_block_with_id(
      Rect::from_xywh(20.0, 5.0, 10.0, 10.0),
      principal_box_id,
      vec![],
    );
    let fragment_pseudo = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, -5.0, 5.0, 5.0),
      pseudo_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(10.0, 20.0, 100.0, 100.0),
      root_box_id,
      vec![fragment_a, fragment_b, fragment_pseudo],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let bounds = absolute_bounds_by_styled_node_id(&box_tree, &fragment_tree);
    assert_eq!(bounds.get(&2).copied(), Some(Rect::from_xywh(10.0, 15.0, 30.0, 20.0)));
  }

  #[test]
  fn absolute_bounds_by_styled_node_id_includes_additional_fragment_roots() {
    let mut child = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    child.styled_node_id = Some(2);

    let mut root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![child],
    );
    root.styled_node_id = Some(1);

    let box_tree = BoxTree::new(root);
    let root_box_id = box_tree.root.id;
    let child_box_id = box_tree.root.children[0].id;

    let mut fragment_tree = FragmentTree::new(FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      root_box_id,
      vec![],
    ));

    let additional_child = FragmentNode::new_block_with_id(
      Rect::from_xywh(3.0, 4.0, 5.0, 6.0),
      child_box_id,
      vec![],
    );
    let additional_root = FragmentNode::new_block(
      Rect::from_xywh(100.0, 200.0, 300.0, 400.0),
      vec![additional_child],
    );
    fragment_tree.additional_fragments.push(additional_root);

    let bounds = absolute_bounds_by_styled_node_id(&box_tree, &fragment_tree);
    assert_eq!(bounds.get(&2).copied(), Some(Rect::from_xywh(103.0, 204.0, 5.0, 6.0)));
  }

  #[test]
  fn absolute_bounds_by_styled_node_id_includes_running_and_footnote_anchor_snapshots() {
    let mut running = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    running.styled_node_id = Some(2);

    let mut footnote = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    footnote.styled_node_id = Some(3);

    let mut root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![running, footnote],
    );
    root.styled_node_id = Some(1);
    let box_tree = BoxTree::new(root);
    let root_box_id = box_tree.root.id;
    let running_box_id = box_tree.root.children[0].id;
    let footnote_box_id = box_tree.root.children[1].id;

    let running_snapshot = FragmentNode::new_block_with_id(
      Rect::from_xywh(5.0, 6.0, 7.0, 8.0),
      running_box_id,
      vec![],
    );
    let running_anchor = FragmentNode::new_running_anchor(
      Rect::from_xywh(10.0, 20.0, 0.0, 0.01),
      "header".to_string(),
      running_snapshot,
    );

    let footnote_snapshot = FragmentNode::new_block_with_id(
      Rect::from_xywh(1.0, 2.0, 3.0, 4.0),
      footnote_box_id,
      vec![],
    );
    let footnote_anchor = FragmentNode::new_footnote_anchor(
      Rect::from_xywh(30.0, 40.0, 0.0, 0.01),
      footnote_snapshot,
      FootnotePolicy::Line,
    );

    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(50.0, 60.0, 100.0, 100.0),
      root_box_id,
      vec![running_anchor, footnote_anchor],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let bounds = absolute_bounds_by_styled_node_id(&box_tree, &fragment_tree);
    assert_eq!(bounds.get(&2).copied(), Some(Rect::from_xywh(65.0, 86.0, 7.0, 8.0)));
    assert_eq!(bounds.get(&3).copied(), Some(Rect::from_xywh(81.0, 102.0, 3.0, 4.0)));
  }
}
