use fastrender::FastRender;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn find_exact_color_bbox(
  pixmap: &tiny_skia::Pixmap,
  target: (u8, u8, u8, u8),
) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut found = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      if pixel(pixmap, x, y) == target {
        found = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  found.then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn inline_containing_block_left_top_are_relative_to_padding_edge_including_border() {
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .wrap { padding: 20px; font-size: 0; line-height: 0; }
  .cb { display: inline; position: relative; border: 10px solid #000; padding: 0; background: #fff; vertical-align: top; }
  .abs { position: absolute; left: 0; top: 0; width: 10px; height: 10px; background: #0f0; }
</style>
<div class="wrap"><span class="cb"><span class="abs"></span></span></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 80, 80).expect("render");

  let green = (0, 255, 0, 255);
  let black = (0, 0, 0, 255);

  let green_bbox = find_exact_color_bbox(&pixmap, green).expect("expected green pixels");
  let black_bbox = find_exact_color_bbox(&pixmap, black).expect("expected black pixels");

  // Border box of cb should start at (20,20) and be 20x20.
  assert_eq!(black_bbox, (20, 20, 39, 39));

  // The containing block is the padding box, so left/top:0 should align with the padding edge,
  // i.e. 10px inside the border.
  assert_eq!(green_bbox, (30, 30, 39, 39));
}
