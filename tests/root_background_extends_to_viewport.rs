use fastrender::{FastRender, RenderArtifactRequest, RenderOptions};

#[test]
fn root_background_gradient_extends_to_viewport_height() {
  // The paint pipeline extends the canvas background to the viewport. If the root background is
  // not extended, pixels below the document content height stay at the renderer background
  // color instead of the CSS background.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      body { background: linear-gradient(180deg, rgb(255, 0, 0) 0%, rgb(0, 0, 255) 100%); }
      .spacer { height: 10px; }
    </style>
    <div class="spacer"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer should construct");
  let viewport_height = 20.0;
  let options = RenderOptions::new().with_viewport(10, viewport_height as u32);
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      "https://example.invalid/",
      options,
      RenderArtifactRequest::default(),
    )
    .expect("render should succeed");
  let pixmap = report.pixmap;
  assert_eq!(pixmap.width(), 10);
  assert_eq!(pixmap.height(), viewport_height as u32);

  let data = pixmap.data();
  let pixel = |x: u32, y: u32| -> (u8, u8, u8, u8) {
    let idx = ((y * pixmap.width() + x) * 4) as usize;
    let b = data[idx];
    let g = data[idx + 1];
    let r = data[idx + 2];
    let a = data[idx + 3];
    (r, g, b, a)
  };

  let (r0, _g0, b0, a0) = pixel(0, 0);
  let (r1, _g1, b1, a1) = pixel(0, pixmap.height() - 1);
  assert_eq!(a0, 255);
  assert_eq!(a1, 255);

  let top_is_red = r0 > 200 && b0 < 80;
  let top_is_blue = b0 > 200 && r0 < 80;
  let bottom_is_red = r1 > 200 && b1 < 80;
  let bottom_is_blue = b1 > 200 && r1 < 80;

  assert!(
    (top_is_red && bottom_is_blue) || (top_is_blue && bottom_is_red),
    "expected extended gradient to reach the bottom of the viewport; got top(r={r0} b={b0}) bottom(r={r1} b={b1})",
  );
}
