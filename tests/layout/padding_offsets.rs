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

#[test]
fn padding_offsets_in_flow_children() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body {
        margin: 0;
        padding: 20px;
        background: #fff;
      }

      .outer {
        padding: 10px;
        background: #fff;
      }

      .inner {
        width: 10px;
        height: 10px;
        background: #000;
      }
    </style>
    <div class="outer">
      <div class="inner"></div>
    </div>
  "#;

  let pixmap = renderer.render_html(html, 100, 100).expect("render");
  let (min_x, min_y, max_x, max_y) = dark_pixel_bounds(&pixmap).expect("expected black box");

  assert_eq!((min_x, min_y), (30, 30));
  assert_eq!((max_x - min_x + 1, max_y - min_y + 1), (10, 10));
}
