//! OOPIF crash isolation integration test.

use fastrender::debug::runtime::RuntimeToggles;
use fastrender::FastRender;
use std::collections::HashMap;

fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let width = pixmap.width();
  let height = pixmap.height();
  assert!(x < width && y < height, "rgba_at out of bounds");
  let idx = (y as usize * width as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn crashed_cross_origin_iframe_renderer_is_isolated_and_shows_placeholder() {
  // Enable out-of-process iframes and point the renderer at the companion binary built by Cargo.
  let mut raw = HashMap::<String, String>::new();
  raw.insert("FASTR_OOPIF".to_string(), "1".to_string());
  raw.insert(
    "FASTR_OOPIF_RENDERER_BIN".to_string(),
    env!("CARGO_BIN_EXE_iframe_renderer").to_string(),
  );
  let toggles = RuntimeToggles::from_map(raw);

  // Root document is HTTPS; iframe uses a different scheme/origin (`crash://`).
  let mut renderer = FastRender::builder()
    .base_url("https://parent.test/")
    .runtime_toggles(toggles)
    .build()
    .expect("build renderer");

  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }
      iframe { display: block; width: 50px; height: 50px; border: 0; margin: 0; padding: 0; }
    </style>
    <iframe src="crash://child.test/"></iframe>
  "#;

  // If the iframe renderer crashes, the root render must still succeed and produce pixels.
  let pixmap = renderer.render_html(html, 80, 60).expect("render root");

  // Root background should still be painted outside of the iframe rect.
  assert_eq!(rgba_at(&pixmap, 70, 50), [0, 0, 255, 255]);

  // The iframe region should show the crash placeholder (checkerboard greys), not the root background.
  let inside = rgba_at(&pixmap, 10, 10);
  assert_eq!(inside[3], 255, "expected placeholder pixel to be opaque");
  assert_ne!(
    inside,
    [0, 0, 255, 255],
    "expected iframe crash placeholder, got root background pixel"
  );
  assert!(
    inside[0] == inside[1] && inside[1] == inside[2],
    "expected grayscale placeholder pixel, got {inside:?}"
  );
  assert!(
    matches!(inside[0], 160 | 210),
    "expected checkerboard placeholder colors (160 or 210), got {inside:?}"
  );
}
