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
fn abspos_left_top_are_relative_to_padding_edge_not_content_edge() {
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .cb { position: relative; padding: 10px; width: 60px; height: 60px; background: #fff; }
  .abs { position: absolute; left: 0; top: 0; width: 10px; height: 10px; background: #0f0; }
</style>
<div class="cb"><div class="abs"></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 80, 80).expect("render");

  let green = (0, 255, 0, 255);
  let bbox = find_exact_color_bbox(&pixmap, green).expect("expected green pixels");

  // In CSS2.1, absolute positioning uses the padding box as the containing block, so left/top:0
  // should align with the padding edge (i.e. the container's border edge when border=0), not the
  // content box (which starts at padding).
  assert_eq!(bbox, (0, 0, 9, 9), "unexpected green bbox: {bbox:?}");
}
