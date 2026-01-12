use fastrender::api::FastRender;

fn dark_pixel_bounds(pixmap: &tiny_skia::Pixmap) -> Option<(usize, usize, usize, usize)> {
  let data = pixmap.data();
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;

  let mut min_x = width;
  let mut min_y = height;
  let mut max_x = 0usize;
  let mut max_y = 0usize;
  let mut any = false;

  for y in 0..height {
    let row_start = y * width * 4;
    for x in 0..width {
      let idx = row_start + x * 4;
      let r = data[idx];
      let g = data[idx + 1];
      let b = data[idx + 2];
      let a = data[idx + 3];

      // Treat any sufficiently dark, non-transparent pixel as part of the box.
      if a > 0 && r < 40 && g < 40 && b < 40 {
        any = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  any.then_some((min_x, min_y, max_x, max_y))
}

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: usize, y: usize) -> (u8, u8, u8, u8) {
  let data = pixmap.data();
  let width = pixmap.width() as usize;
  let idx = (y * width + x) * 4;
  (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
}

#[test]
fn float_right_in_padded_container_uses_container_content_origin() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: #fff; color: #fff; }
      p { margin: 0; }
      .outer {
        padding-left: 46px;
        background: #fff;
      }
      .float {
        float: right;
        width: 50px;
        height: 10px;
        background: #000;
      }
    </style>
    <div class="outer">
      <p><span class="float"></span>text</p>
    </div>
  "#;

  let pixmap = renderer.render_html(html, 300, 50).expect("render");
  let (min_x, _min_y, max_x, _max_y) = dark_pixel_bounds(&pixmap).expect("expected black box");

  assert_eq!(min_x, 250);
  assert_eq!(max_x - min_x + 1, 50);
}

#[test]
fn block_floats_in_centered_container_use_container_content_origin() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: #fff; }
      .container { width: 200px; margin: 0 auto; }
      .a { float: left; width: 100px; height: 10px; background: #000; }
      .b { float: left; width: 100px; height: 10px; background: #f00; }
    </style>
    <div class="container">
      <div class="a"></div>
      <div class="b"></div>
    </div>
  "#;

  let pixmap = renderer.render_html(html, 300, 50).expect("render");

  // The container should be centered: (300 - 200) / 2 = 50.
  let sample_y = 5;

  let (r0, g0, b0, a0) = pixel_rgba(&pixmap, 55, sample_y);
  assert!(
    a0 > 250 && r0 < 20 && g0 < 20 && b0 < 20,
    "expected black at x=55"
  );

  let (r1, g1, b1, a1) = pixel_rgba(&pixmap, 155, sample_y);
  assert!(
    a1 > 250 && r1 > 235 && g1 < 20 && b1 < 20,
    "expected red at x=155"
  );

  let (rw, gw, bw, aw) = pixel_rgba(&pixmap, 5, sample_y);
  assert!(
    aw > 250 && rw > 235 && gw > 235 && bw > 235,
    "expected white at x=5 (float should not escape centered container)"
  );
}
