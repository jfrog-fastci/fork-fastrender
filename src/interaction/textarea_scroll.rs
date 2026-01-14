use crate::geometry::{Rect, Size};
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::style::types::LineHeight;
use crate::style::ComputedStyle;

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + left,
    rect.y() + top,
    (rect.width() - left - right).max(0.0),
    (rect.height() - top - bottom).max(0.0),
  )
}

fn textarea_line_height(viewport_size: Size, style: &ComputedStyle) -> Option<f32> {
  let metrics = if matches!(style.line_height, LineHeight::Normal) {
    super::resolve_scaled_metrics_for_interaction(style)
  } else {
    None
  };
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
  (line_height > 0.0 && line_height.is_finite()).then_some(line_height)
}

fn textarea_text_rect(border_size: Size, style: &ComputedStyle, viewport_size: Size) -> Rect {
  // Mirror the form-control paint path: border box -> content box -> internal 2px inset for text.
  let border_rect = Rect::from_xywh(
    0.0,
    0.0,
    border_size.width.max(0.0),
    border_size.height.max(0.0),
  );
  let content_rect = super::content_rect_for_border_rect(border_rect, style, viewport_size);
  inset_rect(content_rect, 2.0, 2.0, 2.0, 2.0)
}

/// Computes the max vertical scroll offset for `<textarea>` controls given the displayed text.
///
/// This mirrors the textarea paint/wrapping heuristics used by the renderer and scroll wheel logic.
pub(crate) fn textarea_max_scroll_y_for_value(
  viewport_size: Size,
  border_size: Size,
  style: &ComputedStyle,
  value: &str,
) -> Option<f32> {
  let line_height = textarea_line_height(viewport_size, style)?;
  let text_rect = textarea_text_rect(border_size, style, viewport_size);
  let viewport_height = text_rect.height().max(0.0);
  if viewport_height <= 0.0 || !viewport_height.is_finite() {
    return None;
  }

  let chars_per_line = crate::textarea::textarea_chars_per_line(style, text_rect.width());
  let layout = crate::textarea::build_textarea_visual_lines(value, chars_per_line);
  let content_height = layout.lines.len() as f32 * line_height;
  if !content_height.is_finite() {
    return None;
  }

  let max_scroll_y = (content_height - viewport_height).max(0.0);
  max_scroll_y.is_finite().then_some(max_scroll_y)
}

/// Computes a vertical scroll offset that keeps `caret_idx` visible within the textarea viewport.
///
/// Returns the recommended (clamped) scroll y offset, or `None` when geometry cannot be computed.
pub(crate) fn textarea_scroll_y_for_caret(
  viewport_size: Size,
  border_size: Size,
  style: &ComputedStyle,
  value: &str,
  caret_idx: usize,
  current_scroll_y: f32,
) -> Option<f32> {
  let line_height = textarea_line_height(viewport_size, style)?;
  let text_rect = textarea_text_rect(border_size, style, viewport_size);
  let viewport_height = text_rect.height().max(0.0);
  if viewport_height <= 0.0 || !viewport_height.is_finite() {
    return None;
  }

  let max_chars = value.chars().count();
  let caret_idx = caret_idx.min(max_chars);

  let chars_per_line = crate::textarea::textarea_chars_per_line(style, text_rect.width());
  let layout = crate::textarea::build_textarea_visual_lines(value, chars_per_line);
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

  let mut scroll_y = if current_scroll_y.is_finite() {
    current_scroll_y
  } else {
    0.0
  };
  scroll_y = scroll_y.clamp(0.0, max_scroll_y);

  let caret_line_idx =
    crate::textarea::textarea_visual_line_index_for_caret(value, &layout, caret_idx);
  let caret_top = caret_line_idx as f32 * line_height;
  let caret_bottom = caret_top + line_height;

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + viewport_height;

  let mut next_scroll_y = scroll_y;
  if caret_top < viewport_top {
    next_scroll_y = caret_top;
  } else if caret_bottom > viewport_bottom {
    next_scroll_y = caret_bottom - viewport_height;
  }

  if !next_scroll_y.is_finite() {
    next_scroll_y = 0.0;
  }
  Some(next_scroll_y.clamp(0.0, max_scroll_y))
}
