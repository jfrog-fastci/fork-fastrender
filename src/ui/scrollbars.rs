use crate::geometry::{Point, Rect};
use crate::scroll::{ScrollBounds, ScrollState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarAxis {
  Vertical,
  Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarConfig {
  /// Thickness of the scrollbar track/thumb in egui points.
  pub thickness_points: f32,
  /// Inset from the page rect edges in egui points.
  pub padding_points: f32,
  /// Minimum thumb length in egui points.
  pub min_thumb_length_points: f32,
}

impl Default for ScrollbarConfig {
  fn default() -> Self {
    Self {
      thickness_points: 8.0,
      padding_points: 2.0,
      min_thumb_length_points: 24.0,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayScrollbar {
  pub axis: ScrollbarAxis,
  pub track_rect_points: Rect,
  pub thumb_rect_points: Rect,
  /// Minimum scroll offset in CSS pixels used for thumb mapping.
  pub min_scroll_css: f32,
  /// Maximum scroll offset in CSS pixels used for thumb mapping.
  pub max_scroll_css: f32,
  /// Viewport extent in CSS pixels for the corresponding axis.
  pub viewport_extent_css: f32,
}

impl OverlayScrollbar {
  pub fn scroll_range_css(self) -> f32 {
    let range = self.max_scroll_css - self.min_scroll_css;
    if range.is_finite() {
      range.max(0.0)
    } else {
      0.0
    }
  }

  pub fn track_length_points(self) -> f32 {
    let len = match self.axis {
      ScrollbarAxis::Vertical => self.track_rect_points.height(),
      ScrollbarAxis::Horizontal => self.track_rect_points.width(),
    };
    if len.is_finite() {
      len.max(0.0)
    } else {
      0.0
    }
  }

  pub fn thumb_length_points(self) -> f32 {
    let len = match self.axis {
      ScrollbarAxis::Vertical => self.thumb_rect_points.height(),
      ScrollbarAxis::Horizontal => self.thumb_rect_points.width(),
    };
    if len.is_finite() {
      len.max(0.0)
    } else {
      0.0
    }
  }

  pub fn thumb_travel_points(self) -> f32 {
    let travel = self.track_length_points() - self.thumb_length_points();
    if travel.is_finite() {
      travel.max(0.0)
    } else {
      0.0
    }
  }

  /// Convert a thumb drag delta in egui points to a scroll delta in CSS pixels.
  ///
  /// - Positive deltas correspond to scrolling forward (down/right).
  /// - Returns 0 when the scrollbar is not scrollable (e.g. no travel, zero range, non-finite input).
  pub fn scroll_delta_css_for_thumb_drag_points(self, drag_delta_points: f32) -> f32 {
    if drag_delta_points == 0.0 || !drag_delta_points.is_finite() {
      return 0.0;
    }
    let range_css = self.scroll_range_css();
    if range_css <= 0.0 {
      return 0.0;
    }
    let travel_points = self.thumb_travel_points();
    if travel_points <= 0.0 {
      return 0.0;
    }
    let delta_css = drag_delta_points / travel_points * range_css;
    if delta_css.is_finite() { delta_css } else { 0.0 }
  }

  /// If `pos_points` is on the track *outside* the thumb, return a page-up/down delta in CSS pixels.
  ///
  /// This is intended for "click track to page scroll" behaviour.
  pub fn page_delta_css_for_track_click(self, pos_points: Point) -> Option<f32> {
    if !self.track_rect_points.contains_point(pos_points) {
      return None;
    }
    if self.thumb_rect_points.contains_point(pos_points) {
      return None;
    }
    if !self.viewport_extent_css.is_finite() || self.viewport_extent_css <= 0.0 {
      return None;
    }

    let delta = match self.axis {
      ScrollbarAxis::Vertical => {
        if pos_points.y < self.thumb_rect_points.min_y() {
          -self.viewport_extent_css
        } else if pos_points.y > self.thumb_rect_points.max_y() {
          self.viewport_extent_css
        } else {
          return None;
        }
      }
      ScrollbarAxis::Horizontal => {
        if pos_points.x < self.thumb_rect_points.min_x() {
          -self.viewport_extent_css
        } else if pos_points.x > self.thumb_rect_points.max_x() {
          self.viewport_extent_css
        } else {
          return None;
        }
      }
    };
    Some(delta)
  }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OverlayScrollbars {
  pub vertical: Option<OverlayScrollbar>,
  pub horizontal: Option<OverlayScrollbar>,
}

fn sanitize_scroll_bounds(bounds: ScrollBounds) -> ScrollBounds {
  let sanitize_finite = |value: f32| if value.is_finite() { value } else { 0.0 };
  let min_x = sanitize_finite(bounds.min_x).max(0.0);
  let min_y = sanitize_finite(bounds.min_y).max(0.0);
  let max_x = sanitize_finite(bounds.max_x).max(min_x);
  let max_y = sanitize_finite(bounds.max_y).max(min_y);
  ScrollBounds {
    min_x,
    min_y,
    max_x,
    max_y,
  }
}

fn thumb_length_points(track_len_points: f32, viewport_extent_css: f32, scroll_range_css: f32, min_thumb: f32) -> f32 {
  if !track_len_points.is_finite() || track_len_points <= 0.0 {
    return 0.0;
  }
  if !viewport_extent_css.is_finite() || viewport_extent_css <= 0.0 {
    return 0.0;
  }
  if !scroll_range_css.is_finite() || scroll_range_css <= 0.0 {
    return 0.0;
  }

  // "Content" extent includes the viewport plus the scrollable range.
  let content_extent_css = viewport_extent_css + scroll_range_css;
  if !content_extent_css.is_finite() || content_extent_css <= 0.0 {
    return 0.0;
  }

  let ratio = viewport_extent_css / content_extent_css;
  if !ratio.is_finite() || ratio <= 0.0 {
    return 0.0;
  }

  let mut len = track_len_points * ratio;
  if !len.is_finite() {
    len = 0.0;
  }
  let min_thumb = if min_thumb.is_finite() { min_thumb.max(0.0) } else { 0.0 };
  len.clamp(min_thumb, track_len_points)
}

fn thumb_offset_points(track_len_points: f32, thumb_len_points: f32, min_scroll_css: f32, max_scroll_css: f32, scroll_pos_css: f32) -> f32 {
  let range = max_scroll_css - min_scroll_css;
  if !range.is_finite() || range <= 0.0 {
    return 0.0;
  }
  if !track_len_points.is_finite() || !thumb_len_points.is_finite() {
    return 0.0;
  }
  let travel = (track_len_points - thumb_len_points).max(0.0);
  if travel <= 0.0 {
    return 0.0;
  }

  let scroll_pos_css = scroll_pos_css.clamp(min_scroll_css, max_scroll_css);
  let frac = (scroll_pos_css - min_scroll_css) / range;
  if !frac.is_finite() {
    return 0.0;
  }
  (frac.clamp(0.0, 1.0)) * travel
}

pub fn overlay_scrollbars_for_viewport_with_config(
  page_rect_points: Rect,
  viewport_css: (u32, u32),
  scroll_state: &ScrollState,
  scroll_bounds: ScrollBounds,
  config: ScrollbarConfig,
) -> OverlayScrollbars {
  let page_w = page_rect_points.width();
  let page_h = page_rect_points.height();
  if !page_w.is_finite() || !page_h.is_finite() || page_w <= 0.0 || page_h <= 0.0 {
    return OverlayScrollbars::default();
  }

  let viewport_w_css = viewport_css.0 as f32;
  let viewport_h_css = viewport_css.1 as f32;
  if viewport_w_css <= 0.0 || viewport_h_css <= 0.0 {
    return OverlayScrollbars::default();
  }

  let bounds = sanitize_scroll_bounds(scroll_bounds);
  let range_x = bounds.max_x - bounds.min_x;
  let range_y = bounds.max_y - bounds.min_y;

  let thickness = if config.thickness_points.is_finite() {
    config.thickness_points.max(0.0)
  } else {
    0.0
  };
  let padding = if config.padding_points.is_finite() {
    config.padding_points.max(0.0)
  } else {
    0.0
  };
  let min_thumb = config.min_thumb_length_points;

  let show_vertical = range_y.is_finite() && range_y > 0.0 && thickness > 0.0;
  let show_horizontal = range_x.is_finite() && range_x > 0.0 && thickness > 0.0;

  let mut out = OverlayScrollbars::default();

  if show_vertical {
    let corner = if show_horizontal { thickness + padding } else { 0.0 };

    let x0 = page_rect_points.max_x() - padding - thickness;
    let x1 = page_rect_points.max_x() - padding;
    let y0 = page_rect_points.min_y() + padding;
    let y1 = page_rect_points.max_y() - padding - corner;

    let track_w = (x1 - x0).max(0.0);
    let track_h = (y1 - y0).max(0.0);
    if track_w > 0.0 && track_h > 0.0 {
      let track = Rect::from_xywh(x0, y0, track_w, track_h);

      let thumb_h = thumb_length_points(track_h, viewport_h_css, range_y, min_thumb);
      let offset_y = thumb_offset_points(
        track_h,
        thumb_h,
        bounds.min_y,
        bounds.max_y,
        scroll_state.viewport.y,
      );
      let thumb = Rect::from_xywh(track.x(), track.y() + offset_y, track.width(), thumb_h);

      out.vertical = Some(OverlayScrollbar {
        axis: ScrollbarAxis::Vertical,
        track_rect_points: track,
        thumb_rect_points: thumb,
        min_scroll_css: bounds.min_y,
        max_scroll_css: bounds.max_y,
        viewport_extent_css: viewport_h_css,
      });
    }
  }

  if show_horizontal {
    let corner = if show_vertical { thickness + padding } else { 0.0 };

    let x0 = page_rect_points.min_x() + padding;
    let x1 = page_rect_points.max_x() - padding - corner;
    let y0 = page_rect_points.max_y() - padding - thickness;
    let y1 = page_rect_points.max_y() - padding;

    let track_w = (x1 - x0).max(0.0);
    let track_h = (y1 - y0).max(0.0);
    if track_w > 0.0 && track_h > 0.0 {
      let track = Rect::from_xywh(x0, y0, track_w, track_h);

      let thumb_w = thumb_length_points(track_w, viewport_w_css, range_x, min_thumb);
      let offset_x = thumb_offset_points(
        track_w,
        thumb_w,
        bounds.min_x,
        bounds.max_x,
        scroll_state.viewport.x,
      );
      let thumb = Rect::from_xywh(track.x() + offset_x, track.y(), thumb_w, track.height());

      out.horizontal = Some(OverlayScrollbar {
        axis: ScrollbarAxis::Horizontal,
        track_rect_points: track,
        thumb_rect_points: thumb,
        min_scroll_css: bounds.min_x,
        max_scroll_css: bounds.max_x,
        viewport_extent_css: viewport_w_css,
      });
    }
  }

  out
}

pub fn overlay_scrollbars_for_viewport(
  page_rect_points: Rect,
  viewport_css: (u32, u32),
  scroll_state: &ScrollState,
  scroll_bounds: ScrollBounds,
) -> OverlayScrollbars {
  overlay_scrollbars_for_viewport_with_config(
    page_rect_points,
    viewport_css,
    scroll_state,
    scroll_bounds,
    ScrollbarConfig::default(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_approx(actual: f32, expected: f32) {
    let eps = 1e-4;
    assert!(
      (actual - expected).abs() <= eps,
      "actual={actual} expected={expected}"
    );
  }

  fn assert_rect_approx(actual: Rect, expected: Rect) {
    assert_approx(actual.x(), expected.x());
    assert_approx(actual.y(), expected.y());
    assert_approx(actual.width(), expected.width());
    assert_approx(actual.height(), expected.height());
  }

  fn test_config() -> ScrollbarConfig {
    ScrollbarConfig {
      thickness_points: 10.0,
      padding_points: 0.0,
      min_thumb_length_points: 0.0,
    }
  }

  #[test]
  fn scrollbars_vertical_thumb_rect_and_drag_delta() {
    let page_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let viewport_css = (100, 100);
    let scroll_bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 0.0,
      max_y: 900.0,
    };
    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 450.0));

    let bars = overlay_scrollbars_for_viewport_with_config(
      page_rect,
      viewport_css,
      &scroll_state,
      scroll_bounds,
      test_config(),
    );
    let v = bars.vertical.expect("expected vertical scrollbar");

    assert_rect_approx(v.track_rect_points, Rect::from_xywh(90.0, 0.0, 10.0, 100.0));
    assert_rect_approx(v.thumb_rect_points, Rect::from_xywh(90.0, 45.0, 10.0, 10.0));

    let delta_css = v.scroll_delta_css_for_thumb_drag_points(9.0);
    assert_approx(delta_css, 90.0);
  }

  #[test]
  fn scrollbars_hide_when_not_scrollable() {
    let page_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let viewport_css = (100, 100);
    let scroll_bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 0.0,
      max_y: 0.0,
    };
    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 0.0));

    let bars = overlay_scrollbars_for_viewport_with_config(
      page_rect,
      viewport_css,
      &scroll_state,
      scroll_bounds,
      test_config(),
    );
    assert!(bars.vertical.is_none());
    assert!(bars.horizontal.is_none());
  }

  #[test]
  fn scrollbars_avoid_corner_overlap_when_both_visible() {
    let page_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let viewport_css = (100, 100);
    let scroll_bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 900.0,
      max_y: 900.0,
    };
    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 0.0));

    let bars = overlay_scrollbars_for_viewport_with_config(
      page_rect,
      viewport_css,
      &scroll_state,
      scroll_bounds,
      test_config(),
    );
    let v = bars.vertical.expect("expected vertical scrollbar");
    let h = bars.horizontal.expect("expected horizontal scrollbar");

    assert_rect_approx(v.track_rect_points, Rect::from_xywh(90.0, 0.0, 10.0, 90.0));
    assert_rect_approx(h.track_rect_points, Rect::from_xywh(0.0, 90.0, 90.0, 10.0));
  }

  #[test]
  fn scrollbars_track_click_pages_by_viewport_extent() {
    let page_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let viewport_css = (100, 100);
    let scroll_bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 0.0,
      max_y: 900.0,
    };
    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 450.0));

    let bars = overlay_scrollbars_for_viewport_with_config(
      page_rect,
      viewport_css,
      &scroll_state,
      scroll_bounds,
      test_config(),
    );
    let v = bars.vertical.expect("expected vertical scrollbar");

    let page_up = v.page_delta_css_for_track_click(Point::new(95.0, 0.0));
    assert_eq!(page_up, Some(-100.0));
    let page_down = v.page_delta_css_for_track_click(Point::new(95.0, 99.0));
    assert_eq!(page_down, Some(100.0));
  }
}

