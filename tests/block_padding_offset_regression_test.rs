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
fn absolute_positioned_border_box_uses_border_origin() {
  // Regression test for absolutely positioned blocks: AbsoluteLayout computes positions in the
  // content-box coordinate space, but fragment bounds are stored as border boxes. Mixing those
  // coordinate spaces would shift the entire box by its own border+padding.
  //
  // Assert at the pixel level so we don't depend on the internal fragment tree shape (anonymous
  // boxes, etc.).
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .outer {
    position: absolute;
    left: 0;
    top: 0;
    border: 4px solid #000;
    padding: 16px;
    background: #fff;
  }
  .inner { width: 10px; height: 10px; background: #0f0; }
</style>
<div class="outer"><div class="inner"></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 64, 64).expect("render");

  let black = (0, 0, 0, 255);
  let green = (0, 255, 0, 255);

  let black_bbox = find_exact_color_bbox(&pixmap, black).expect("expected black border pixels");
  let green_bbox =
    find_exact_color_bbox(&pixmap, green).expect("expected green pixels to be painted");

  // Border box should start at (0,0) and be 50x50 (4px border + 16px padding + 10px content).
  assert_eq!(
    black_bbox,
    (0, 0, 49, 49),
    "unexpected black bbox: {black_bbox:?}"
  );

  // Inner content should start at border(4) + padding(16) = 20px and be 10x10.
  assert_eq!(
    green_bbox,
    (20, 20, 29, 29),
    "unexpected green bbox: {green_bbox:?}"
  );
}
