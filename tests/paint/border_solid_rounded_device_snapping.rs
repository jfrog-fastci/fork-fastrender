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
    (
      outside.red(),
      outside.green(),
      outside.blue(),
      outside.alpha()
    ),
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

#[test]
fn solid_rounded_border_half_pixel_edges_snap_consistently() {
  // Regression test for half-integer geometry: many layouts (notably Bootstrap-based pages with
  // `0.75rem` padding at an 18px root font size) produce border boxes that start/end on `.5`
  // device pixels.
  //
  // Chrome/Skia's non-AA fill snapping treats pixel centers on the min edge as outside and pixel
  // centers on the max edge as inside, so a left edge at `2.5px` should start at device pixel `3`
  // (not `2`).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #ff6600; }
      #box {
        box-sizing: border-box;
        position: absolute;
        left: 2.5px;
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

  let outside_left = pixmap.pixel(2, 10).expect("outside-left pixel");
  assert_eq!(
    (
      outside_left.red(),
      outside_left.green(),
      outside_left.blue(),
      outside_left.alpha()
    ),
    (255, 102, 0, 255),
    "expected pixel outside border to remain the body background color"
  );

  let border_left = pixmap.pixel(3, 10).expect("left border pixel");
  assert_eq!(
    (
      border_left.red(),
      border_left.green(),
      border_left.blue(),
      border_left.alpha()
    ),
    (255, 255, 255, 255),
    "expected left border pixel to be solid white"
  );

  let inside_left = pixmap.pixel(4, 10).expect("inside-left pixel");
  assert_eq!(
    (
      inside_left.red(),
      inside_left.green(),
      inside_left.blue(),
      inside_left.alpha()
    ),
    (0, 255, 0, 255),
    "expected pixel inside border to match element background color"
  );

  let inside_right = pixmap.pixel(19, 10).expect("inside-right pixel");
  assert_eq!(
    (
      inside_right.red(),
      inside_right.green(),
      inside_right.blue(),
      inside_right.alpha()
    ),
    (0, 255, 0, 255),
    "expected pixel inside border to match element background color"
  );

  let border_right = pixmap.pixel(20, 10).expect("right border pixel");
  assert_eq!(
    (
      border_right.red(),
      border_right.green(),
      border_right.blue(),
      border_right.alpha()
    ),
    (255, 255, 255, 255),
    "expected right border pixel to be solid white"
  );

  let outside_right = pixmap.pixel(21, 10).expect("outside-right pixel");
  assert_eq!(
    (
      outside_right.red(),
      outside_right.green(),
      outside_right.blue(),
      outside_right.alpha()
    ),
    (255, 102, 0, 255),
    "expected pixel outside border to remain the body background color"
  );
}
