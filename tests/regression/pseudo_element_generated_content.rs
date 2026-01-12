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
fn pseudo_element_generated_content_paints_box_for_empty_string_content() {
  // Regression guard: `::before`/`::after` generated content should participate in layout/paint even
  // when the `content` value is an empty string.
  //
  // Avoid asserting on text glyph rasterization; instead generate a fixed-size colored box so the
  // test is stable across font changes.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .host { width: 20px; height: 20px; background: #fff; }
  .host::before { content: ''; display: block; width: 10px; height: 10px; background: #f00; }
</style>
<div class="host"></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 32, 32).expect("render");

  let red = (255, 0, 0, 255);
  let red_bbox = find_exact_color_bbox(&pixmap, red).expect("expected red pixels");
  assert_eq!(red_bbox, (0, 0, 9, 9), "unexpected red bbox: {red_bbox:?}");
}

