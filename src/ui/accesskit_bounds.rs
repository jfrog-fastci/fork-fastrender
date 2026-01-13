#![cfg(feature = "browser_ui")]

/// Helpers for converting between egui's coordinate space (points) and the coordinate system used
/// by the AccessKit nodes emitted by egui.
///
/// Why this exists:
/// - egui layouts widgets in "points" (logical pixels).
/// - AccessKit bounds are used for screen-reader highlight rectangles and spatial navigation.
/// - When we inject additional AccessKit nodes (e.g. for rendered page content), their bounds must
///   be in the same coordinate system that egui emits, otherwise the highlight rectangles will be
///   misaligned, especially on HiDPI displays.
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
pub fn accesskit_rect_from_egui_rect(rect_points: EguiRect, pixels_per_point: f32) -> accesskit::Rect {
  let ppp = sanitize_pixels_per_point(pixels_per_point) as f64;
  let scale = if ACCESSKIT_BOUNDS_ARE_PHYSICAL_PIXELS { ppp } else { 1.0 };

  accesskit::Rect::new(
    rect_points.min.x as f64 * scale,
    rect_points.min.y as f64 * scale,
    rect_points.max.x as f64 * scale,
    rect_points.max.y as f64 * scale,
  )
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
}
