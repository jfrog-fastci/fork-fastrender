use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, RenderOptions};

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn legacy_paint_backend_toggles() -> RuntimeToggles {
  // Don't inherit host `FASTR_*` env vars; tests should be deterministic under the unified
  // integration test binary.
  let mut raw = std::collections::HashMap::<String, String>::new();
  raw.insert("FASTR_PAINT_BACKEND".to_string(), "legacy".to_string());
  RuntimeToggles::from_map(raw)
}

fn count_dark_row_bands(pixmap: &tiny_skia::Pixmap) -> usize {
  let mut bands = 0usize;
  let mut in_band = false;
  for y in 0..pixmap.height() {
    let mut has_dark = false;
    for x in 0..pixmap.width() {
      let (r, g, b, a) = pixel_rgba(pixmap, x, y);
      if a > 0 && r < 160 && g < 160 && b < 160 {
        has_dark = true;
        break;
      }
    }
    if has_dark {
      if !in_band {
        bands += 1;
        in_band = true;
      }
    } else {
      in_band = false;
    }
  }
  bands
}

#[test]
fn missing_image_placeholder_does_not_flood_fill_replaced_box() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; }
      .bg { width: 100px; height: 100px; background: rgb(0, 255, 0); }
      img { display: block; width: 100px; height: 100px; }
    </style>
    <div class="bg"><img src="data:image/png;base64,"></div>
  "#;

  let pixmap = renderer.render_html(html, 100, 100).expect("render");
  let (r, g, b, _) = pixel_rgba(&pixmap, 50, 50);
  assert!(
    g > r + 80 && g > b + 80 && g > 80,
    "expected missing image to keep background visible (r={r}, g={g}, b={b})"
  );
}

#[test]
fn missing_image_alt_text_wraps_within_replaced_box() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
     <style>
       body { margin: 0; font-family: sans-serif; font-size: 16px; line-height: 24px; color: rgb(0, 0, 0); }
      img { display: block; width: 60px; height: 60px; }
    </style>
    <img src="data:image/png;base64," alt="This is a long alt text that should wrap onto multiple lines">
  "#;

  let pixmap = renderer.render_html(html, 60, 60).expect("render");
  let bands = count_dark_row_bands(&pixmap);
  assert!(
    bands >= 2,
    "expected wrapped alt text to paint multiple line bands, got {bands}"
  );
}

#[test]
fn missing_image_placeholder_paints_broken_image_icon_in_legacy_painter() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; }
      .bg { width: 40px; height: 40px; background: rgb(0, 255, 0); }
      img { display: block; width: 40px; height: 40px; }
    </style>
    <div class="bg"><img src="data:image/png;base64,"></div>
  "#;

  let options = RenderOptions::default()
    .with_viewport(40, 40)
    .with_runtime_toggles(legacy_paint_backend_toggles());
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  // Ensure the broken-image icon colors are painted (not just transparent).
  assert_eq!(pixel_rgba(&pixmap, 4, 4), (198, 216, 244, 255), "sky");
  assert_eq!(pixel_rgba(&pixmap, 4, 12), (255, 255, 255, 255), "background");
  assert_eq!(pixel_rgba(&pixmap, 4, 15), (88, 174, 57, 255), "ground");

  // Ensure the rest of the image box stays transparent so author backgrounds remain visible.
  let (r, g, b, _) = pixel_rgba(&pixmap, 30, 30);
  assert!(
    g > r + 80 && g > b + 80 && g > 80,
    "expected missing image to keep background visible (r={r}, g={g}, b={b})"
  );
}
