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
fn absolute_descendant_in_inline_flow_positions_against_padding_edge_of_positioned_block() {
  // Regression test: absolutely positioned descendants that appear inside inline content should be
  // positioned relative to the padding edge of the nearest positioned ancestor. When the inline
  // formatting context runs in the ancestor's content coordinate space, the padding edge is
  // outside the content box, so the containing block origin must account for padding instead of
  // double-counting it after block layout translates fragments into border-box space.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .parent {
    position: relative;
    border: 4px solid #000;
    padding: 16px;
    background: #fff;
    color: #fff; /* ensure inline text doesn't introduce black pixels */
  }
  .abs {
    position: absolute;
    left: 5px;
    top: 7px;
    width: 6px;
    height: 6px;
    background: #f00;
  }
</style>
<div class="parent">
  <span>hello <span class="abs"></span> world</span>
</div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 80, 80).expect("render");

  let red = (255, 0, 0, 255);
  let red_bbox = find_exact_color_bbox(&pixmap, red).expect("expected red pixels to be painted");

  // Offsets resolve against the padding edge: border-left 4 + left 5 = 9; border-top 4 + top 7 = 11.
  assert_eq!(red_bbox, (9, 11, 14, 16), "unexpected red bbox: {red_bbox:?}");
}

