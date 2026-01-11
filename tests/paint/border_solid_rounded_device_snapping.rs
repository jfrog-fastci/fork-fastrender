use super::util::create_stacking_context_bounds_renderer;

#[test]
fn solid_rounded_border_edges_snap_to_device_pixels() {
  // Regression test for solid rounded borders: the display-list renderer draws uniform solid
  // rounded borders as an even-odd filled rounded-rect ring. Without device-pixel snapping, the
  // straight border edges can land on fractional pixels and become anti-aliased, producing washed
  // out 1px borders and subtle bleed into adjacent pixels.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #ff6600; }
      #box {
        box-sizing: border-box;
        position: absolute;
        left: 2.8px;
        top: 2px;
        width: 18px;
        height: 18px;
        background: #0f0;
        border: 1px solid #fff;
        border-radius: 8px 0 0 8px;
      }
    </style>
    <div id="box"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  let outside = pixmap.pixel(21, 10).expect("outside pixel");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue(), outside.alpha()),
    (255, 102, 0, 255),
    "expected pixel outside border to remain the body background color"
  );

  let border = pixmap.pixel(20, 10).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected right border pixel to be solid white"
  );

  let inside = pixmap.pixel(19, 10).expect("inside pixel");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue(), inside.alpha()),
    (0, 255, 0, 255),
    "expected pixel inside border to match element background color"
  );
}
