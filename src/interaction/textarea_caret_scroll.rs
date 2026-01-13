use crate::dom::DomNode;
use crate::geometry::{Point, Rect};
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::scroll::ScrollState;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType, FormControlKind, ReplacedType};
use crate::tree::fragment_tree::FragmentTree;
use std::sync::Arc;

use super::fragment_geometry::content_rect_for_border_rect;
use super::InteractionState;

const TEXT_INSET_CSS: f32 = 2.0;
const CARET_SCROLL_PADDING_CSS: f32 = 2.0;

fn inset_rect_uniform(rect: Rect, inset: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + inset,
    rect.y() + inset,
    (rect.width() - inset * 2.0).max(0.0),
    (rect.height() - inset * 2.0).max(0.0),
  )
}

fn textarea_control_snapshot_from_box_tree(
  box_tree: &BoxTree,
  textarea_node_id: usize,
) -> Option<(usize, Arc<ComputedStyle>)> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(textarea_node_id) {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(form_control) = &replaced.replaced_type {
          if matches!(form_control.control, FormControlKind::TextArea { .. }) {
            return Some((node.id, node.style.clone()));
          }
        }
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn border_rect_for_box_id(fragment_tree: &FragmentTree, target_box_id: usize) -> Option<Rect> {
  struct Frame<'a> {
    node: &'a crate::tree::fragment_tree::FragmentNode,
    abs_origin: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    node: &fragment_tree.root,
    abs_origin: fragment_tree.root.bounds.origin,
  });
  for root in &fragment_tree.additional_fragments {
    stack.push(Frame {
      node: root,
      abs_origin: root.bounds.origin,
    });
  }

  while let Some(Frame { node, abs_origin }) = stack.pop() {
    if node.box_id() == Some(target_box_id) {
      return Some(Rect::from_xywh(
        abs_origin.x,
        abs_origin.y,
        node.bounds.width(),
        node.bounds.height(),
      ));
    }

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        abs_origin: abs_origin.translate(child.bounds.origin),
      });
    }
  }

  None
}

fn clamp_padding(padding: f32, viewport_height: f32) -> f32 {
  if !padding.is_finite() || !viewport_height.is_finite() || viewport_height <= 0.0 {
    return 0.0;
  }
  padding.max(0.0).min(viewport_height * 0.5)
}

/// Computes the target textarea `scrollTop` (in CSS px) needed to keep the focused caret visible.
///
/// Returns `(textarea_box_id, next_scroll_y)` when a focused `<textarea>` caret is present and
/// scrolling should change.
pub(crate) fn textarea_scroll_y_to_reveal_focused_caret(
  dom: &mut DomNode,
  interaction_state: &InteractionState,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
) -> Option<(usize, f32)> {
  let focused = interaction_state.focused?;
  let caret = interaction_state.text_edit_for(focused)?.caret;

  let (textarea_box_id, style) = textarea_control_snapshot_from_box_tree(box_tree, focused)?;
  let border_rect = border_rect_for_box_id(fragment_tree, textarea_box_id)?;

  let viewport_size = fragment_tree.viewport_size();
  let style = style.as_ref();
  let content_rect = content_rect_for_border_rect(border_rect, style, viewport_size);
  let text_rect = inset_rect_uniform(content_rect, TEXT_INSET_CSS);
  if !(text_rect.width() > 0.0 && text_rect.height() > 0.0) {
    return None;
  }

  let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
    super::resolve_scaled_metrics_for_interaction(style)
  } else {
    None
  };
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
  if !line_height.is_finite() || line_height <= 0.0 {
    return None;
  }

  let node = crate::dom::find_node_mut_by_preorder_id(dom, focused)?;
  let value = crate::dom::textarea_current_value(node);

  let chars_per_line = crate::textarea::textarea_chars_per_line(style, text_rect.width());
  let layout = crate::textarea::build_textarea_visual_lines(&value, chars_per_line);
  let line_idx = crate::textarea::textarea_visual_line_index_for_caret(&value, &layout, caret);

  let viewport_height = text_rect.height().max(0.0);
  if !viewport_height.is_finite() || viewport_height <= 0.0 {
    return None;
  }

  let content_height = layout.lines.len() as f32 * line_height;
  if !content_height.is_finite() {
    return None;
  }

  let max_scroll_y = (content_height - viewport_height).max(0.0);
  let max_scroll_y = if max_scroll_y.is_finite() {
    max_scroll_y
  } else {
    0.0
  };

  let existing_y = scroll_state.element_offset(textarea_box_id).y;
  let existing_y = if existing_y.is_finite() {
    existing_y.max(0.0)
  } else {
    0.0
  };
  let current_y = existing_y.clamp(0.0, max_scroll_y);

  let band_start = line_idx as f32 * line_height;
  let band_end = band_start + line_height;
  if !band_start.is_finite() || !band_end.is_finite() {
    return None;
  }

  let padding = clamp_padding(CARET_SCROLL_PADDING_CSS, viewport_height);
  let padded_start = padding;
  let padded_end = (viewport_height - padding).max(padded_start);
  let visible_start = current_y + padded_start;
  let visible_end = current_y + padded_end;

  let mut desired = current_y;
  if band_start < visible_start && band_end <= visible_end {
    desired = band_start - padded_start;
  } else if band_end > visible_end && band_start >= visible_start {
    desired = band_end - padded_end;
  }
  if !desired.is_finite() {
    desired = current_y;
  }

  desired = desired.clamp(0.0, max_scroll_y);
  if !desired.is_finite() {
    desired = current_y;
  }

  (desired != existing_y).then_some((textarea_box_id, desired))
}
