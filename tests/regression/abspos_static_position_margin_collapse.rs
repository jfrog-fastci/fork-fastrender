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
fn absolute_static_position_respects_collapsed_vertical_margins() {
  // Static position is defined as the position the element would have had in normal flow.
  // For block-level siblings, vertical margins collapse (CSS 2.1 §8.3.1), and the static position
  // should reflect the *collapsed* margin, not the sum of the previous sibling's margin-bottom and
  // this element's margin-top.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .cb { position: relative; width: 64px; height: 64px; background: #fff; }
  .flow { height: 10px; margin: 0 0 10px 0; background: #f00; }
  .abs {
    position: absolute;
    left: 0;
    width: 10px;
    height: 10px;
    margin-top: 20px;
    background: #0f0;
  }
</style>
<div class="cb">
  <div class="flow"></div>
  <div class="abs"></div>
</div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 64, 64).expect("render");

  let green = (0, 255, 0, 255);
  let green_bbox =
    find_exact_color_bbox(&pixmap, green).expect("expected green pixels to be painted");

  // The first in-flow block occupies y=[0,9] and has a 10px bottom margin. The absolute element has
  // margin-top:20px. Those vertical margins collapse to 20px, so the abspos border box should start
  // at y=10+20=30.
  assert_eq!(
    green_bbox,
    (0, 30, 9, 39),
    "unexpected green bbox: {green_bbox:?}"
  );
}
