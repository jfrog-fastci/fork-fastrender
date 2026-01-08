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
fn stretch_keyword_sizes_inline_block_to_available_width() {
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .container { width: 50px; font-size: 0; line-height: 0; }
  .child { display: inline-block; width: stretch; height: 10px; background: #f00; }
</style>
<div class="container"><div class="child"></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 64, 20).expect("render");

  let red = (255, 0, 0, 255);
  let red_bbox = find_exact_color_bbox(&pixmap, red).expect("expected red pixels to be painted");
  assert_eq!(red_bbox, (0, 0, 49, 9), "unexpected red bbox: {red_bbox:?}");
}

