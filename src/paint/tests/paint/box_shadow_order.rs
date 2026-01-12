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
fn box_shadow_paints_first_shadow_on_top() {
  // The first shadow in the `box-shadow` list should be painted on top of subsequent shadows.
  //
  // Use crisp (non-blurred) shadows with different spread radii so the rings overlap, making the
  // paint order observable via a single pixel sample.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 30px;
        top: 30px;
        width: 40px;
        height: 40px;
        box-shadow:
          0 0 0 10px rgb(255, 0, 0),
          0 0 0 20px rgb(0, 0, 255);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  // 5px outside the border box: inside both shadows, so should be red (first shadow on top).
  assert_eq!(
    color_at(&pixmap, 25, 50),
    [255, 0, 0, 255],
    "expected first box-shadow (red) to paint on top of second shadow in the overlap region"
  );

  // 15px outside the border box: inside only the second shadow, so should be blue.
  assert_eq!(
    color_at(&pixmap, 15, 50),
    [0, 0, 255, 255],
    "expected second box-shadow (blue) to be visible outside the overlap region"
  );
}
