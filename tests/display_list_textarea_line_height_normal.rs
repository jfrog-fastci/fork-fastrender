use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn count_red(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
        total += 1;
      }
    }
  }
  total
}

fn red_bounds(pixmap: &Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut any = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
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
fn display_list_textarea_line_height_normal_positions_caret_using_font_metrics() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // The `mvar-metrics-test.ttf` fixture has weight-dependent MVAR deltas that make `line-height:
  // normal` much larger at the heavy instance, so the second line should be clipped out of this
  // textarea and the caret should remain on the first line.
  let html = "<!doctype html>\
    <style>\
      @font-face{font-family:\"VarMVAR\";src:url(\"tests/fixtures/fonts/mvar-metrics-test.ttf\") format(\"truetype\");font-weight:100 900;}\
      html,body{margin:0;background:black;}\
    </style>\
    <textarea data-fastr-focus=\"true\" style=\"display:block;margin:0;width:220px;height:65px;min-height:0;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-family:'VarMVAR';font-size:50px;font-weight:900;font-variation-settings:'wght' 900;line-height:normal;\">A\n</textarea>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 240, 120)
    .expect("render textarea");

  let Some((_min_x, min_y, _max_x, _max_y)) = red_bounds(&pixmap) else {
    panic!("expected caret to paint in red pixels");
  };

  let total_red = count_red(&pixmap, 0, 0, 240, 120);

  // With incorrect `line-height: normal` handling, the caret is pushed to the second (clipped)
  // line, leaving all red pixels below the first line.
  let top_red = count_red(&pixmap, 0, 0, 240, 40);
  assert!(
    top_red > 0,
    "expected caret to appear on the first line when line-height is computed from font metrics (min_y={min_y}, top_red={top_red}, total_red={total_red})"
  );
}
