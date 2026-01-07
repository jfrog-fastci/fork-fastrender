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
      if px.alpha() > 200 && px.red() > 200 && px.green() < 80 && px.blue() < 80 {
        total += 1;
      }
    }
  }
  total
}

fn count_green(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.green() > 200 && px.red() < 80 && px.blue() < 80 {
        total += 1;
      }
    }
  }
  total
}

#[test]
fn legacy_range_pseudo_element_styles_are_used() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>\
      html,body{margin:0;background:black;}\
      input{display:block;margin:0;width:120px;height:24px;box-sizing:content-box;border:0;padding:0;}\
      input::-webkit-slider-runnable-track{height:6px;background:rgb(255,0,0);border:0;}\
      input::-webkit-slider-thumb{width:20px;height:20px;background:rgb(0,255,0);border:0;}\
    </style>\
    <input type=range min=0 max=100 value=50>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(html, 140, 40).expect("render range");

  // The range input is positioned at (0,0) with a 120x24 content box, and value=50 centers the
  // thumb at x=60, y=12. The pseudo thumb background should paint as green.
  let green = count_green(&pixmap, 55, 7, 65, 17);
  assert!(green > 0, "expected green pixels in thumb center region");

  // The pseudo track background should paint as red. Sample the right half beyond the filled
  // accent portion (which ends at x=60).
  let red = count_red(&pixmap, 90, 9, 119, 15);
  assert!(red > 0, "expected red pixels on the unfilled track region");
}

