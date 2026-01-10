use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render html")
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_gray_strip(pixmap: &Pixmap, strip_x: u32, expected: u8) {
  // Sample a few pixels with different (x, y) mod 4 values to ensure we're not getting an ordered
  // dither pattern.
  for (dx, dy) in [(1, 1), (2, 2), (3, 3)] {
    assert_eq!(
      color_at(pixmap, strip_x + dx, dy),
      (expected, expected, expected, 255),
      "unexpected pixel in strip starting at x={strip_x} (dx={dx}, dy={dy})"
    );
  }
}

#[test]
fn opacity_layers_source_over_match_chrome_without_dither() {
  // In Chrome/Skia, compositing an opaque black layer with `opacity: X` over an opaque white
  // backdrop yields uniform pixels. The layer opacity is quantized to an 8-bit alpha (rounded),
  // then composed with integer `mul/255` arithmetic.
  //
  // These values are intentionally strict: large translucent overlays dominate page diffs when the
  // compositor introduces a ±1 checkerboard dither pattern.
  let html = r#"
    <style>
      html, body { margin: 0; width: 100%; height: 100%; background: white; }
      body { position: relative; }
      .strip { position: absolute; top: 0; bottom: 0; width: 10px; background: black; }
      #s1 { left: 0px; opacity: 0.1; }
      #s2 { left: 10px; opacity: 0.3; }
      #s3 { left: 20px; opacity: 0.5; }
      #s4 { left: 30px; opacity: 0.75; }
    </style>
    <div id="s1" class="strip"></div>
    <div id="s2" class="strip"></div>
    <div id="s3" class="strip"></div>
    <div id="s4" class="strip"></div>
  "#;

  let pixmap = render(html, 40, 8);

  // Expected output (confirmed against Chrome):
  //   out = 255 - round(opacity * 255)
  assert_gray_strip(&pixmap, 0, 229); // opacity 0.1 -> alpha 26
  assert_gray_strip(&pixmap, 10, 178); // opacity 0.3 -> alpha 77
  assert_gray_strip(&pixmap, 20, 127); // opacity 0.5 -> alpha 128
  assert_gray_strip(&pixmap, 30, 64); // opacity 0.75 -> alpha 191
}

