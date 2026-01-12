use crate::geometry::{Point, Rect};
use crate::scroll::{ScrollBounds, ScrollState};
use std::time::{Duration, Instant};

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
    if delta_css.is_finite() {
      delta_css
    } else {
      0.0
    }
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

/// Timing configuration for overlay scrollbar visibility/fade behaviour.
///
/// This is used by the windowed UI (`src/bin/browser.rs`) to mimic modern browser overlay scrollbars
/// (show on scroll/hover/drag, then fade out after a short idle period).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayScrollbarVisibilityConfig {
  /// Duration of the fade-in animation once the scrollbar becomes visible.
  pub fade_in_duration: Duration,
  /// How long to stay fully visible after the most recent interaction (scroll/hover/drag).
  pub idle_duration: Duration,
  /// Duration of the fade-out animation after `idle_duration` expires.
  pub fade_out_duration: Duration,
  /// Target frame interval for fade animations (egui doesn't drive this automatically in our
  /// windowed UI; we schedule winit wakeups manually).
  pub frame_interval: Duration,
  /// Minimum opacity applied during fade-in so the first frame is still visible.
  ///
  /// Without this, starting a fade-in at `t=0` would yield alpha=0.0, and the scrollbar might not
  /// be visible at all for very short scroll interactions (single wheel tick).
  pub fade_in_min_alpha: f32,
}

impl Default for OverlayScrollbarVisibilityConfig {
  fn default() -> Self {
    Self {
      fade_in_duration: Duration::from_millis(120),
      idle_duration: Duration::from_millis(600),
      fade_out_duration: Duration::from_millis(240),
      frame_interval: Duration::from_millis(16),
      fade_in_min_alpha: 0.2,
    }
  }
}

/// Minimal UI state for overlay scrollbar visibility.
///
/// The windowed UI keeps this in `App` so scrollbars can fade in/out even when the page isn't
/// actively repainting (requires scheduling winit wakeups).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OverlayScrollbarVisibilityState {
  /// When the scrollbar became visible (for fade-in).
  pub visible_since: Option<Instant>,
  /// Last time the user interacted in a way that should keep scrollbars visible.
  pub last_interaction: Option<Instant>,
}

impl OverlayScrollbarVisibilityState {
  pub fn register_interaction(&mut self, now: Instant) {
    if self.visible_since.is_none() {
      self.visible_since = Some(now);
    }
    self.last_interaction = Some(now);
  }

  pub fn alpha(
    self,
    now: Instant,
    config: OverlayScrollbarVisibilityConfig,
    force_visible: bool,
  ) -> f32 {
    let Some(visible_since) = self.visible_since else {
      return 0.0;
    };

    let elapsed_in = now.saturating_duration_since(visible_since);
    let fade_in = if config.fade_in_duration.is_zero() {
      1.0
    } else {
      let t = (elapsed_in.as_secs_f32() / config.fade_in_duration.as_secs_f32()).clamp(0.0, 1.0);
      let min = config.fade_in_min_alpha.clamp(0.0, 1.0);
      min + (1.0 - min) * t
    };

    let fade_out = if force_visible {
      1.0
    } else {
      let Some(last) = self.last_interaction else {
        return 0.0;
      };
      let elapsed_since_interaction = now.saturating_duration_since(last);
      if elapsed_since_interaction <= config.idle_duration {
        1.0
      } else if config.fade_out_duration.is_zero() {
        0.0
      } else {
        let fade_elapsed = elapsed_since_interaction - config.idle_duration;
        if fade_elapsed >= config.fade_out_duration {
          0.0
        } else {
          let t = (fade_elapsed.as_secs_f32() / config.fade_out_duration.as_secs_f32())
            .clamp(0.0, 1.0);
          1.0 - t
        }
      }
    };

    (fade_in * fade_out).clamp(0.0, 1.0)
  }

  /// Returns true when the caller should request a repaint at `now` to advance a fade animation.
  pub fn needs_repaint(
    self,
    now: Instant,
    config: OverlayScrollbarVisibilityConfig,
    force_visible: bool,
  ) -> bool {
    let Some(visible_since) = self.visible_since else {
      return false;
    };

    if !config.fade_in_duration.is_zero()
      && now.saturating_duration_since(visible_since) < config.fade_in_duration
    {
      return true;
    }

    if force_visible {
      return false;
    }

    let Some(last) = self.last_interaction else {
      return false;
    };

    let elapsed = now.saturating_duration_since(last);
    elapsed >= config.idle_duration && elapsed <= (config.idle_duration + config.fade_out_duration)
  }

  /// Returns the next winit wakeup deadline needed to advance overlay scrollbar animations.
  pub fn next_wakeup(
    self,
    now: Instant,
    config: OverlayScrollbarVisibilityConfig,
    force_visible: bool,
  ) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    let mut consider = |candidate: Instant| {
      next = Some(match next {
        Some(existing) => existing.min(candidate),
        None => candidate,
      });
    };

    let Some(visible_since) = self.visible_since else {
      return None;
    };

    // Fade-in needs periodic repaints until complete.
    let fade_in_end = visible_since + config.fade_in_duration;
    if !config.fade_in_duration.is_zero() && now < fade_in_end {
      let candidate = (now + config.frame_interval).min(fade_in_end);
      consider(candidate);
    }

    if !force_visible {
      if let Some(last) = self.last_interaction {
        let fade_out_start = last + config.idle_duration;
        if now < fade_out_start {
          // Wake at the moment fade-out starts so we can request a redraw even if the UI is idle.
          consider(fade_out_start);
        } else {
          let fade_out_end = fade_out_start + config.fade_out_duration;
          if now < fade_out_end {
            let candidate = (now + config.frame_interval).min(fade_out_end);
            consider(candidate);
          }
        }
      }
    }

    next
  }
}

fn sanitize_scroll_bounds(bounds: ScrollBounds) -> ScrollBounds {
  // Scroll bounds can legitimately be negative: e.g. when content extends above/left of the scroll
  // origin due to negative positioning/margins.
  //
  // Keep negative mins so overlay scrollbar thumbs can represent "scroll further up/left" states.
  // Only sanitize non-finite values and ensure max >= min.
  let sanitize_min = |value: f32| if value.is_finite() { value } else { 0.0 };
  let min_x = sanitize_min(bounds.min_x);
  let min_y = sanitize_min(bounds.min_y);

  let sanitize_max = |value: f32, min: f32| {
    let value = if value.is_finite() { value } else { min };
    value.max(min)
  };
  let max_x = sanitize_max(bounds.max_x, min_x);
  let max_y = sanitize_max(bounds.max_y, min_y);
  ScrollBounds {
    min_x,
    min_y,
    max_x,
    max_y,
  }
}

fn thumb_length_points(
  track_len_points: f32,
  viewport_extent_css: f32,
  scroll_range_css: f32,
  min_thumb: f32,
) -> f32 {
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
  let min_thumb = if min_thumb.is_finite() {
    min_thumb.max(0.0)
  } else {
    0.0
  };
  len.clamp(min_thumb, track_len_points)
}

fn thumb_offset_points(
  track_len_points: f32,
  thumb_len_points: f32,
  min_scroll_css: f32,
  max_scroll_css: f32,
  scroll_pos_css: f32,
) -> f32 {
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
    let corner = if show_horizontal {
      thickness + padding
    } else {
      0.0
    };

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
    let corner = if show_vertical {
      thickness + padding
    } else {
      0.0
    };

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
  use std::time::{Duration, Instant};

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

  #[test]
  fn scrollbars_support_negative_min_scroll_bounds() {
    // Root scroll bounds can be negative when content extends above the scroll origin. Overlay
    // scrollbars should reflect that by positioning the thumb below the top when scroll=0.
    let page_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let viewport_css = (100, 100);
    let scroll_bounds = ScrollBounds {
      min_x: 0.0,
      min_y: -100.0,
      max_x: 0.0,
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

    // With min_y < 0, scroll_y=0 is not the topmost possible scroll position. The thumb should be
    // offset downward relative to the track top.
    assert!(v.thumb_rect_points.y() > v.track_rect_points.y());
    // The expected value is derived from the mapping math:
    // - range = 1000 (900 - -100)
    // - content extent = 1100 (viewport 100 + range 1000)
    // - ratio = 100/1100 => thumb_len=9.0909, travel=90.9091
    // - scroll=0 => frac=(0 - -100)/1000 = 0.1 => offset=9.0909
    assert_approx(v.thumb_rect_points.y(), 9.090909);
  }

  fn visibility_test_config() -> OverlayScrollbarVisibilityConfig {
    OverlayScrollbarVisibilityConfig {
      fade_in_duration: Duration::from_millis(100),
      idle_duration: Duration::from_millis(200),
      fade_out_duration: Duration::from_millis(100),
      frame_interval: Duration::from_millis(16),
      fade_in_min_alpha: 0.2,
    }
  }

  #[test]
  fn overlay_scrollbar_visibility_alpha_fade_in_and_out() {
    let cfg = visibility_test_config();
    let t0 = Instant::now();
    let mut state = OverlayScrollbarVisibilityState::default();

    assert_eq!(state.alpha(t0, cfg, false), 0.0);

    state.register_interaction(t0);
    assert_approx(state.alpha(t0, cfg, false), cfg.fade_in_min_alpha);

    // Halfway through fade-in: alpha should interpolate from `fade_in_min_alpha` → 1.0.
    let a_half_in = state.alpha(t0 + Duration::from_millis(50), cfg, false);
    assert_approx(a_half_in, 0.6);

    // Fade-in complete: fully visible.
    assert_approx(state.alpha(t0 + Duration::from_millis(100), cfg, false), 1.0);

    // Still fully visible during idle.
    assert_approx(state.alpha(t0 + Duration::from_millis(150), cfg, false), 1.0);

    // Halfway through fade-out (starts at t0+200ms, lasts 100ms).
    let a_half_out = state.alpha(t0 + Duration::from_millis(250), cfg, false);
    assert_approx(a_half_out, 0.5);

    // Fade-out complete.
    assert_approx(state.alpha(t0 + Duration::from_millis(300), cfg, false), 0.0);
  }

  #[test]
  fn overlay_scrollbar_visibility_schedules_wakeups() {
    let cfg = visibility_test_config();
    let t0 = Instant::now();
    let mut state = OverlayScrollbarVisibilityState::default();

    state.register_interaction(t0);

    // During fade-in, schedule a near-future frame.
    assert_eq!(state.next_wakeup(t0, cfg, false), Some(t0 + cfg.frame_interval));

    // Once fade-in is done, the next wakeup should be the fade-out start.
    assert_eq!(
      state.next_wakeup(t0 + Duration::from_millis(120), cfg, false),
      Some(t0 + cfg.idle_duration)
    );

    // During fade-out, schedule a near-future frame.
    let during_fade_out = t0 + cfg.idle_duration + Duration::from_millis(10);
    assert_eq!(
      state.next_wakeup(during_fade_out, cfg, false),
      Some(during_fade_out + cfg.frame_interval)
    );
  }

  #[test]
  fn overlay_scrollbar_visibility_force_visible_suppresses_fade_out() {
    let cfg = visibility_test_config();
    let t0 = Instant::now();
    let mut state = OverlayScrollbarVisibilityState::default();

    state.register_interaction(t0);

    // Even after idle+fade_out, force_visible keeps the scrollbar visible.
    assert_approx(state.alpha(t0 + Duration::from_millis(500), cfg, true), 1.0);
    // With force_visible, we don't need to wake up once fade-in has completed.
    assert_eq!(
      state.next_wakeup(t0 + Duration::from_millis(200), cfg, true),
      None
    );
  }
}
