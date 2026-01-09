use super::util::create_stacking_context_bounds_renderer_legacy;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  renderer.render_html(html, width, height).expect("render")
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn stacking_context_viewport_culling_uses_inverse_transform() {
  // Regression for viewport culling in the legacy painter: for transformed stacking contexts, the
  // visible region must be computed in source space via the inverse transform. Otherwise, an
  // offscreen-pre-transform element that is translated into view can be incorrectly culled and
  // render as empty.
  //
  // The element is 300px wide in a 100px viewport. With `left: 50%` and `translateX(-50%)` it
  // should fully cover the viewport after the transform.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        top: 0;
        left: 50%;
        width: 300px;
        height: 100px;
        background: rgb(255, 0, 0);
        transform: translateX(-50%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  for (x, y) in [(0, 0), (10, 10), (50, 50), (99, 99)] {
    let (r, g, b, a) = rgba_at(&pixmap, x, y);
    assert!(
      r > 200 && g < 50 && b < 50 && a == 255,
      "expected red pixel at ({x},{y}), got rgba=({r},{g},{b},{a})"
    );
  }
}

#[test]
fn stacking_context_background_image_culling_accounts_for_layer_origin() {
  // Similar to the transform viewport-culling regression above, but specifically exercises
  // background image painting. Background layers must be culled against the painter's pixmap
  // bounds in CSS coordinates (origin_offset + pixmap size), not the global viewport 0..WxH.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        top: 0;
        left: 50%;
        width: 300px;
        height: 100px;
        transform: translateX(-50%);
        background-image: linear-gradient(to right, rgb(255, 0, 0), rgb(0, 0, 255));
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  let (lr, lg, lb, la) = rgba_at(&pixmap, 10, 50);
  let (rr, rg, rb, ra) = rgba_at(&pixmap, 90, 50);

  assert!(la == 255 && ra == 255, "expected opaque pixels");
  assert!(
    lr > lb,
    "expected left side of gradient to be redder than blue (got rgba=({lr},{lg},{lb},{la}))"
  );
  assert!(
    rb > rr,
    "expected right side of gradient to be bluer than red (got rgba=({rr},{rg},{rb},{ra}))"
  );
}
