#![cfg(feature = "browser_ui")]

//! AccessKit bounds conversion helpers.
//!
//! FastRender produces layout geometry in **CSS pixels** (typically viewport-local). The windowed
//! browser UI uses egui for chrome; egui lays out widgets in **points** (logical pixels) and emits
//! AccessKit nodes with bounds in that same coordinate space.
//!
//! When we inject additional AccessKit nodes (e.g. for rendered page content), their bounds must be
//! expressed in the same coordinate space expected by `accesskit_winit`/egui so screen-reader
//! highlight rectangles are aligned, including on HiDPI displays.

use egui::Rect as EguiRect;

/// Returns `true` if the AccessKit bounds in `egui::PlatformOutput::accesskit_update` are expressed
/// in *physical pixels* (i.e. already multiplied by `pixels_per_point`).
///
/// This constant is validated by a unit test (see below).
const ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS: bool = false;

fn sanitize_pixels_per_point(pixels_per_point: f32) -> f32 {
  if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
    pixels_per_point
  } else {
    1.0
  }
}

/// Convert an egui rectangle (in points) into an AccessKit rectangle in the coordinate space used
/// by egui's emitted AccessKit nodes.
pub fn accesskit_rect_from_egui_rect(
  rect_points: EguiRect,
  pixels_per_point: f32,
) -> accesskit::Rect {
  let ppp = sanitize_pixels_per_point(pixels_per_point) as f64;
  let scale = if ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS { ppp } else { 1.0 };

  accesskit::Rect::new(
    rect_points.min.x as f64 * scale,
    rect_points.min.y as f64 * scale,
    rect_points.max.x as f64 * scale,
    rect_points.max.y as f64 * scale,
  )
}

/// Converts viewport-local FastRender CSS-pixel rectangles into AccessKit `Rect`s.
///
/// Coordinate spaces:
/// - **Input**: `rect_css` is in **CSS pixels** with an origin at the top-left of the *page viewport*
///   (i.e. viewport-local).
/// - **Output**: `accesskit::Rect` in the coordinate space expected by `accesskit_winit` (and by
///   egui's `accesskit_update` output).
///
/// Transform:
/// - `offset` is the **placement** of the page viewport origin inside the window's **inner**
///   coordinate space, expressed in *CSS pixels*.
///   - Example: if the page is rendered below the browser chrome, `offset.y` is the chrome height
///     (in CSS px) and `offset.x` is usually 0.
/// - `scale` is the CSS px → AccessKit coordinate scale factor.
///   - When interoperating with egui's `accesskit_update`, this is usually `1.0` because egui emits
///     AccessKit bounds in points (see `ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccessKitBoundsTransform {
  pub offset: (f64, f64),
  pub scale: f64,
}

impl AccessKitBoundsTransform {
  /// Convert a FastRender `Rect` (CSS px) into an AccessKit `Rect`.
  ///
  /// Returns `None` if any input value is non-finite.
  ///
  /// Negative widths/heights are clamped to 0 before conversion.
  pub fn transform_rect(&self, rect_css: crate::geometry::Rect) -> Option<accesskit::Rect> {
    if !(self.offset.0.is_finite()
      && self.offset.1.is_finite()
      && self.scale.is_finite()
      && rect_css.origin.x.is_finite()
      && rect_css.origin.y.is_finite()
      && rect_css.size.width.is_finite()
      && rect_css.size.height.is_finite())
    {
      return None;
    }

    let w_css = rect_css.size.width.max(0.0) as f64;
    let h_css = rect_css.size.height.max(0.0) as f64;
    let x_css = rect_css.origin.x as f64;
    let y_css = rect_css.origin.y as f64;

    let x0 = (x_css + self.offset.0) * self.scale;
    let y0 = (y_css + self.offset.1) * self.scale;
    let x1 = (x_css + w_css + self.offset.0) * self.scale;
    let y1 = (y_css + h_css + self.offset.1) * self.scale;

    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
      return None;
    }

    Some(accesskit::Rect { x0, y0, x1, y1 })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-3
  }

  fn rect_matches_scaled(rect_points: EguiRect, bounds: &accesskit::Rect, scale: f64) -> bool {
    approx_eq(bounds.x0, rect_points.min.x as f64 * scale)
      && approx_eq(bounds.y0, rect_points.min.y as f64 * scale)
      && approx_eq(bounds.x1, rect_points.max.x as f64 * scale)
      && approx_eq(bounds.y1, rect_points.max.y as f64 * scale)
  }

  #[test]
  fn egui_accesskit_bounds_are_in_points_not_physical_pixels() {
    // This test intentionally uses a non-1.0 `pixels_per_point` so we can detect whether egui's
    // AccessKit bounds are scaled to physical pixels or left in points.
    let pixels_per_point = 2.0;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut button_rect_points: Option<EguiRect> = None;

    let mut raw = egui::RawInput::default();
    raw.pixels_per_point = Some(pixels_per_point);
    raw.screen_rect = Some(EguiRect::from_min_size(
      egui::Pos2::ZERO,
      egui::vec2(400.0, 200.0),
    ));

    ctx.begin_frame(raw);
    egui::CentralPanel::default().show(&ctx, |ui| {
      let resp = ui.button("Hello");
      button_rect_points = Some(resp.rect);
    });
    let output = ctx.end_frame();

    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected egui to emit an AccessKit update");

    let button_node = update
      .nodes
      .iter()
      .find_map(|(_id, node)| {
        (node.role() == accesskit::Role::Button && node.name() == Some("Hello")).then_some(node)
      })
      .expect("expected to find a Button node named \"Hello\" in AccessKit update");

    let bounds = button_node
      .bounds()
      .expect("expected button node to include bounds")
      .clone();

    let rect_points = button_rect_points.expect("expected button rect to be set");

    let matches_points = rect_matches_scaled(rect_points, &bounds, 1.0);
    let matches_physical =
      rect_matches_scaled(rect_points, &bounds, sanitize_pixels_per_point(pixels_per_point) as f64);
    assert!(
      matches_points ^ matches_physical,
      "expected AccessKit bounds to match either points or physical pixels scaling, not both/neither.\n  rect_points={rect_points:?}\n  bounds={bounds:?}\n  pixels_per_point={pixels_per_point}"
    );

    // Current egui 0.23 emits bounds in points (not multiplied by `pixels_per_point`).
    assert!(
      matches_points,
      "expected egui AccessKit bounds to be in points (unscaled), but they appear to be scaled to physical pixels.\n  rect_points={rect_points:?}\n  bounds={bounds:?}\n  pixels_per_point={pixels_per_point}"
    );

    assert!(
      !ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS,
      "test expectation mismatch: update ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS to reflect egui output"
    );
  }

  #[test]
  fn transform_rect_applies_offset_and_scale() {
    let tx = AccessKitBoundsTransform {
      offset: (10.0, 20.0),
      scale: 2.0,
    };

    let rect_css = crate::geometry::Rect::from_xywh(1.0, 2.0, 3.0, 4.0);
    let out = tx.transform_rect(rect_css).expect("rect should be finite");
    assert_eq!(
      out,
      accesskit::Rect {
        x0: 22.0,
        y0: 44.0,
        x1: 28.0,
        y1: 52.0
      }
    );
  }

  #[test]
  fn transform_rect_returns_none_for_non_finite_input() {
    let tx = AccessKitBoundsTransform {
      offset: (0.0, 0.0),
      scale: 1.0,
    };

    let rect_css = crate::geometry::Rect::from_xywh(f32::NAN, 0.0, 10.0, 10.0);
    assert_eq!(tx.transform_rect(rect_css), None);
  }

  #[test]
  fn transform_rect_clamps_negative_dimensions() {
    let tx = AccessKitBoundsTransform {
      offset: (0.0, 0.0),
      scale: 1.0,
    };

    let rect_css = crate::geometry::Rect::from_xywh(5.0, 7.0, -10.0, -2.0);
    let out = tx.transform_rect(rect_css).expect("rect should be finite");
    assert_eq!(
      out,
      accesskit::Rect {
        x0: 5.0,
        y0: 7.0,
        x1: 5.0,
        y1: 7.0
      }
    );
  }
}

