use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn assert_pixel_rgba_approx(
  pixmap: &Pixmap,
  x: u32,
  y: u32,
  expected: (u8, u8, u8, u8),
  tolerance: u8,
) {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
  let got = (p.red(), p.green(), p.blue(), p.alpha());
  assert!(
    got.0.abs_diff(expected.0) <= tolerance
      && got.1.abs_diff(expected.1) <= tolerance
      && got.2.abs_diff(expected.2) <= tolerance
      && got.3.abs_diff(expected.3) <= tolerance,
    "pixel at ({x},{y}) expected rgba{:?} (tol={tolerance}), got rgba{got:?}",
    expected
  );
}

#[test]
fn css2_clip_rect_is_applied_in_final_paint_output() {
  let mut renderer = create_stacking_context_bounds_renderer();
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }

      /* CSS 2.1 `clip` only applies to abs/fixed positioned elements. */
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        background: rgb(255, 0, 0);
        clip: rect(0px, 20px, 20px, 0px);
      }

      /* Optional extra coverage: verify `clip` does not apply to non-abspos. */
      #container { position: absolute; left: 0; top: 60px; }
      #nonabs {
        width: 40px;
        height: 40px;
        background: rgb(0, 0, 255);
        clip: rect(0px, 20px, 20px, 0px);
      }
    </style>
    <div id="target"></div>
    <div id="container"><div id="nonabs"></div></div>
  "#;

  let pixmap = renderer.render_html(html, 100, 120).expect("render");
  // These probes are far from the clip edges, but use a small tolerance to avoid flakiness from
  // future compositing/premultiplication changes.
  const TOL: u8 = 2;

  // Inside the clipped 20x20 region.
  assert_pixel_rgba_approx(&pixmap, 10, 10, (255, 0, 0, 255), TOL);

  // Outside the clip to the right/below: should reveal the white body background.
  assert_pixel_rgba_approx(&pixmap, 30, 10, (255, 255, 255, 255), TOL);
  assert_pixel_rgba_approx(&pixmap, 10, 30, (255, 255, 255, 255), TOL);

  // `clip` should not apply to non-abspos elements; this probe would be clipped otherwise.
  assert_pixel_rgba_approx(&pixmap, 30, 70, (0, 0, 255, 255), TOL);
}
