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
fn absolute_static_position_does_not_double_count_margin_left() {
  // Absolutely positioned boxes use the "static position" when both left/right are auto.
  // That static position is based on the hypothetical in-flow margin edge; margins are then
  // applied by the absolute positioning constraint equation.
  //
  // A common failure mode is to treat the in-flow border box x-position as the static position
  // and then *also* add the resolved margin, effectively doubling it.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .cb { position: relative; width: 64px; height: 64px; background: #fff; }
  .abs {
    position: absolute;
    width: 10px;
    height: 10px;
    margin-left: 20px;
    background: #0f0;
  }
</style>
<div class="cb"><div class="abs"></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 64, 64).expect("render");

  let green = (0, 255, 0, 255);
  let green_bbox =
    find_exact_color_bbox(&pixmap, green).expect("expected green pixels to be painted");

  // The border box should start at x=20 and be 10px wide.
  assert_eq!(
    green_bbox,
    (20, 0, 29, 9),
    "unexpected green bbox: {green_bbox:?}"
  );
}
