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

#[test]
fn legacy_form_control_caret_color_is_used() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,0,0);}</style>\
    <input data-fastr-focus=\"true\" value=\"\" style=\"display:block;margin:0;width:40px;height:40px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-size:30px;line-height:1;\">";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render form control");

  // Caret should be near the left edge of the focused input.
  let caret_red = count_red(&pixmap, 0, 0, 12, 50);
  assert!(caret_red > 0, "expected caret to paint in red pixels");

  // Ensure no other red pixels exist outside of the caret region.
  let total_red = count_red(&pixmap, 0, 0, 100, 100);
  assert_eq!(
    total_red, caret_red,
    "expected caret to be the only red pixels (total_red={total_red}, caret_red={caret_red})"
  );
}

