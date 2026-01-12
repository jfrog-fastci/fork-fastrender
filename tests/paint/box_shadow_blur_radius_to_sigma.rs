use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn blurred_box_shadow_uses_css_blur_radius_semantics() {
  // `blur-radius: 10px` in CSS is not interpreted as the Gaussian sigma directly; browsers treat
  // it as a larger radius and convert to sigma when rasterizing.
  //
  // Regression test: if we treat the CSS blur radius as sigma, the shadow tail stays
  // significantly darker than Chromium for common values (e.g. nginx.org header shadow).
  let html = r#"
    <style>
      html, body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 20px;
        box-shadow: 0 5px 10px black;
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 200, 60);

  // Near the shadow, pixels should be visibly darkened (not pure white).
  let near = color_at(&pixmap, 100, 27);
  assert!(
    near[0] < 230 && near[1] < 230 && near[2] < 230,
    "expected shadow to darken pixels near the box, got {near:?}"
  );

  // Further away, the shadow should decay toward white quickly (matching browser behavior).
  let far = color_at(&pixmap, 100, 37);
  assert!(
    far[0] > 240 && far[1] > 240 && far[2] > 240,
    "expected blurred shadow tail to be close to white, got {far:?}"
  );

  assert!(
    near[0] < far[0],
    "expected shadow to get lighter with distance, near={near:?} far={far:?}"
  );
}
