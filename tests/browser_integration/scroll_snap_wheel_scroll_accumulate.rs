use super::support::deterministic_renderer;
use fastrender::{BrowserDocument, Point, RenderOptions, Result};

#[test]
fn scroll_snap_accumulates_wheel_scroll_deltas_before_paint() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          html { scroll-snap-type: y mandatory; }
          .snap { height: 100px; scroll-snap-align: start; }
        </style>
      </head>
      <body>
        <div class="snap"></div>
        <div class="snap"></div>
        <div class="snap"></div>
        <div class="snap"></div>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(100, 100);
  let mut doc = BrowserDocument::new(deterministic_renderer(), html, options)?;

  // Ensure we have cached layout artifacts for wheel hit-testing.
  doc.render_frame_with_scroll_state()?;

  let viewport_point = Point::new(10.0, 10.0);

  // Two small deltas should accumulate (80px total) before snapping is applied during paint.
  let changed1 = doc.wheel_scroll_at_viewport_point(viewport_point, (0.0, 40.0))?;
  let changed2 = doc.wheel_scroll_at_viewport_point(viewport_point, (0.0, 40.0))?;
  assert!(changed1, "expected first wheel scroll to update the scroll state");
  assert!(changed2, "expected second wheel scroll to update the scroll state");

  let raw_y = doc.scroll_state().viewport.y;
  assert!(
    (raw_y - 80.0).abs() < 1.0,
    "expected wheel deltas to accumulate to ~80px before snapping, got {raw_y}"
  );

  // Paint applies scroll snapping, which should now land at the next target (~100px).
  let frame = doc.render_frame_with_scroll_state()?;
  let snapped_y = frame.scroll_state.viewport.y;
  assert!(
    (snapped_y - 100.0).abs() < 1.0,
    "expected scroll snap to land at ~100px after paint, got {snapped_y}"
  );

  Ok(())
}

