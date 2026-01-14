use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect};
use crate::scroll::{ScrollBounds, ScrollState};

fn sanitize_f32(value: f32) -> f32 {
  if value.is_finite() {
    value
  } else {
    0.0
  }
}

fn sanitize_non_negative_f32(value: f32) -> f32 {
  sanitize_f32(value).max(0.0)
}

/// IPC-safe 2D point using `f32` coordinates.
///
/// This is intentionally separate from the engine's [`crate::geometry::Point`] type so IPC message
/// formats remain small and stable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PointF32 {
  pub x: f32,
  pub y: f32,
}

impl PointF32 {
  pub fn sanitize(self) -> Self {
    Self {
      x: sanitize_f32(self.x),
      y: sanitize_f32(self.y),
    }
  }
}

impl From<Point> for PointF32 {
  fn from(point: Point) -> Self {
    Self {
      x: point.x,
      y: point.y,
    }
    .sanitize()
  }
}

impl From<PointF32> for Point {
  fn from(point: PointF32) -> Self {
    let point = point.sanitize();
    Point::new(point.x, point.y)
  }
}

/// IPC-safe axis-aligned rectangle.
///
/// Uses the `x/y/w/h` representation (origin + size) to match many existing call sites.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RectF32 {
  pub x: f32,
  pub y: f32,
  pub w: f32,
  pub h: f32,
}

impl RectF32 {
  pub fn sanitize(self) -> Self {
    Self {
      x: sanitize_f32(self.x),
      y: sanitize_f32(self.y),
      w: sanitize_non_negative_f32(self.w),
      h: sanitize_non_negative_f32(self.h),
    }
  }
}

impl From<Rect> for RectF32 {
  fn from(rect: Rect) -> Self {
    Self {
      x: rect.origin.x,
      y: rect.origin.y,
      w: rect.size.width,
      h: rect.size.height,
    }
    .sanitize()
  }
}

impl From<RectF32> for Rect {
  fn from(rect: RectF32) -> Self {
    let rect = rect.sanitize();
    Rect::from_xywh(rect.x, rect.y, rect.w, rect.h)
  }
}

/// IPC-safe subset of scroll state.
///
/// This intentionally only includes the viewport scroll offset; element scroll offsets are stored
/// in maps keyed by internal ids which are not stable across processes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScrollStateIpc {
  pub viewport: PointF32,
}

impl ScrollStateIpc {
  pub fn sanitize(self) -> Self {
    let viewport = self.viewport.sanitize();
    Self {
      // Our scroll model clamps scroll offsets to non-negative values (matching browser behavior).
      viewport: PointF32 {
        x: viewport.x.max(0.0),
        y: viewport.y.max(0.0),
      },
    }
  }
}

impl From<&ScrollState> for ScrollStateIpc {
  fn from(scroll: &ScrollState) -> Self {
    Self {
      viewport: scroll.viewport.into(),
    }
    .sanitize()
  }
}

impl From<ScrollStateIpc> for ScrollState {
  fn from(scroll: ScrollStateIpc) -> Self {
    let scroll = scroll.sanitize();
    ScrollState::with_viewport(scroll.viewport.into())
  }
}

/// IPC-safe scroll sizing information for the root scroll container (viewport).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScrollMetricsIpc {
  pub viewport_css: (u32, u32),
  pub scroll_css: (f32, f32),
  pub bounds_css: (f32, f32, f32, f32),
  pub content_css: (f32, f32),
}

impl ScrollMetricsIpc {
  pub fn sanitize(self) -> Self {
    let scroll_x = sanitize_non_negative_f32(self.scroll_css.0);
    let scroll_y = sanitize_non_negative_f32(self.scroll_css.1);
    let content_w = sanitize_non_negative_f32(self.content_css.0);
    let content_h = sanitize_non_negative_f32(self.content_css.1);

    let bounds_min_x = sanitize_non_negative_f32(self.bounds_css.0);
    let bounds_min_y = sanitize_non_negative_f32(self.bounds_css.1);
    let bounds_max_x = sanitize_non_negative_f32(self.bounds_css.2).max(bounds_min_x);
    let bounds_max_y = sanitize_non_negative_f32(self.bounds_css.3).max(bounds_min_y);

    Self {
      viewport_css: self.viewport_css,
      scroll_css: (scroll_x, scroll_y),
      bounds_css: (bounds_min_x, bounds_min_y, bounds_max_x, bounds_max_y),
      content_css: (content_w, content_h),
    }
  }
}

#[cfg(feature = "vmjs")]
impl From<crate::ui::messages::ScrollMetrics> for ScrollMetricsIpc {
  fn from(metrics: crate::ui::messages::ScrollMetrics) -> Self {
    let bounds = metrics.bounds_css;
    Self {
      viewport_css: metrics.viewport_css,
      scroll_css: metrics.scroll_css,
      bounds_css: (bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y),
      content_css: metrics.content_css,
    }
    .sanitize()
  }
}

#[cfg(feature = "vmjs")]
impl From<ScrollMetricsIpc> for crate::ui::messages::ScrollMetrics {
  fn from(metrics: ScrollMetricsIpc) -> Self {
    let metrics = metrics.sanitize();
    crate::ui::messages::ScrollMetrics {
      viewport_css: metrics.viewport_css,
      scroll_css: metrics.scroll_css,
      bounds_css: ScrollBounds {
        min_x: metrics.bounds_css.0,
        min_y: metrics.bounds_css.1,
        max_x: metrics.bounds_css.2,
        max_y: metrics.bounds_css.3,
      },
      content_css: metrics.content_css,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
    value: &T,
  ) {
    let json = serde_json::to_string(value).unwrap();
    let decoded: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&decoded, value);
  }

  #[test]
  fn point_f32_roundtrip_serde() {
    assert_roundtrip(&PointF32 { x: 1.25, y: -2.5 });
  }

  #[test]
  fn rect_f32_roundtrip_serde() {
    assert_roundtrip(&RectF32 {
      x: 1.0,
      y: 2.0,
      w: 3.0,
      h: 4.0,
    });
  }

  #[test]
  fn scroll_state_ipc_roundtrip_serde() {
    assert_roundtrip(&ScrollStateIpc {
      viewport: PointF32 { x: 10.0, y: 20.0 },
    });
  }

  #[test]
  fn scroll_metrics_ipc_roundtrip_serde() {
    assert_roundtrip(&ScrollMetricsIpc {
      viewport_css: (800, 600),
      scroll_css: (12.5, 7.0),
      bounds_css: (0.0, 0.0, 100.0, 200.0),
      content_css: (900.0, 1200.0),
    });
  }

  #[test]
  fn point_f32_sanitize_clamps_nan_and_infinite() {
    let sanitized = PointF32 {
      x: f32::NAN,
      y: f32::INFINITY,
    }
    .sanitize();
    assert_eq!(sanitized, PointF32 { x: 0.0, y: 0.0 });

    let json = serde_json::to_string(&sanitized).unwrap();
    assert!(json.contains("0.0"));
  }

  #[test]
  fn rect_f32_sanitize_clamps_non_finite_and_negative_sizes() {
    let sanitized = RectF32 {
      x: f32::NEG_INFINITY,
      y: f32::NAN,
      w: -10.0,
      h: f32::INFINITY,
    }
    .sanitize();
    assert_eq!(
      sanitized,
      RectF32 {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
      }
    );
  }

  #[test]
  fn scroll_state_ipc_sanitize_clamps_negative_and_non_finite_offsets() {
    let sanitized = ScrollStateIpc {
      viewport: PointF32 { x: -5.0, y: f32::NAN },
    }
    .sanitize();
    assert_eq!(
      sanitized,
      ScrollStateIpc {
        viewport: PointF32 { x: 0.0, y: 0.0 },
      }
    );
  }

  #[test]
  fn scroll_metrics_ipc_sanitize_clamps_non_finite_and_negative_values() {
    let sanitized = ScrollMetricsIpc {
      viewport_css: (1, 1),
      scroll_css: (-1.0, f32::NAN),
      bounds_css: (f32::NAN, -2.0, -3.0, f32::INFINITY),
      content_css: (f32::NEG_INFINITY, 4.0),
    }
    .sanitize();
    assert_eq!(
      sanitized,
      ScrollMetricsIpc {
        viewport_css: (1, 1),
        scroll_css: (0.0, 0.0),
        bounds_css: (0.0, 0.0, 0.0, 0.0),
        content_css: (0.0, 4.0),
      }
    );
  }
}
