use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render_display_list(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn linear_gradient_dither_phase_is_anchored_to_device_pixels() {
  // Regression for gentoo.org: linear-gradient backgrounds use ordered dithering, and the dither
  // matrix phase must be anchored to device pixel coordinates (not shifted by one row).
  let html = r#"
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        left: 0;
        top: 64px;
        width: 128px;
        height: 40px;
        background-image: linear-gradient(to bottom, rgb(84, 72, 122) 0%, rgb(73, 63, 106) 100%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render_display_list(html, 200, 120);

  // At device y=70 (6px into the gradient), Chrome/Skia's 8×8 Bayer matrix yields alternating
  // low/high dither thresholds across x, producing these two colors at x=0 and x=1.
  assert_eq!(rgba_at(&pixmap, 0, 70), (82, 70, 119, 255));
  assert_eq!(rgba_at(&pixmap, 1, 70), (83, 71, 120, 255));
}

