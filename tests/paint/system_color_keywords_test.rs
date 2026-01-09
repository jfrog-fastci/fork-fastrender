use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_with_prefers_color_scheme(pref: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
    ("FASTR_PREFERS_COLOR_SCHEME".to_string(), pref.to_string()),
  ]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>\
      html { color-scheme: light dark; }\
      html,body{margin:0;}\
      #box{width:20px;height:20px;background-color:Canvas;color:CanvasText;border:2px solid currentColor;box-sizing:border-box;}\
    </style>\
    <div id=box></div>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 32, 32).expect("render html")
}

fn render_gradient_with_prefers_color_scheme(pref: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
    ("FASTR_PREFERS_COLOR_SCHEME".to_string(), pref.to_string()),
  ]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>\
      html { color-scheme: light dark; }\
      html,body{margin:0;}\
      #box{width:20px;height:20px;\
        background-image:linear-gradient(to right, Canvas 0%, Canvas 50%, CanvasText 50%, CanvasText 100%);\
      }\
    </style>\
    <div id=box></div>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 32, 32).expect("render html")
}

#[test]
fn system_color_keywords_paint_using_palette() {
  let light = render_with_prefers_color_scheme("light");
  let dark = render_with_prefers_color_scheme("dark");

  let light_border = light.pixel(1, 10).expect("light border pixel");
  assert_eq!(
    (light_border.red(), light_border.green(), light_border.blue(), light_border.alpha()),
    (0, 0, 0, 255),
    "expected border to be CanvasText in light scheme"
  );
  let light_bg = light.pixel(3, 10).expect("light background pixel");
  assert_eq!(
    (light_bg.red(), light_bg.green(), light_bg.blue(), light_bg.alpha()),
    (255, 255, 255, 255),
    "expected background to be Canvas in light scheme"
  );

  let dark_border = dark.pixel(1, 10).expect("dark border pixel");
  assert_eq!(
    (dark_border.red(), dark_border.green(), dark_border.blue(), dark_border.alpha()),
    (232, 232, 232, 255),
    "expected border to be CanvasText in dark scheme"
  );
  let dark_bg = dark.pixel(3, 10).expect("dark background pixel");
  assert_eq!(
    (dark_bg.red(), dark_bg.green(), dark_bg.blue(), dark_bg.alpha()),
    (16, 16, 16, 255),
    "expected background to be Canvas in dark scheme"
  );

  assert_ne!(
    (light_border.red(), light_border.green(), light_border.blue()),
    (dark_border.red(), dark_border.green(), dark_border.blue()),
    "expected CanvasText to differ between schemes"
  );
  assert_ne!(
    (light_bg.red(), light_bg.green(), light_bg.blue()),
    (dark_bg.red(), dark_bg.green(), dark_bg.blue()),
    "expected Canvas to differ between schemes"
  );
}

#[test]
fn system_color_keywords_resolve_inside_gradients() {
  let light = render_gradient_with_prefers_color_scheme("light");
  let dark = render_gradient_with_prefers_color_scheme("dark");

  let light_left = light.pixel(2, 10).expect("light left pixel");
  assert_eq!(
    (
      light_left.red(),
      light_left.green(),
      light_left.blue(),
      light_left.alpha()
    ),
    (255, 255, 255, 255),
    "expected Canvas in light scheme"
  );
  let light_right = light.pixel(18, 10).expect("light right pixel");
  assert_eq!(
    (
      light_right.red(),
      light_right.green(),
      light_right.blue(),
      light_right.alpha()
    ),
    (0, 0, 0, 255),
    "expected CanvasText in light scheme"
  );

  let dark_left = dark.pixel(2, 10).expect("dark left pixel");
  assert_eq!(
    (dark_left.red(), dark_left.green(), dark_left.blue(), dark_left.alpha()),
    (16, 16, 16, 255),
    "expected Canvas in dark scheme"
  );
  let dark_right = dark.pixel(18, 10).expect("dark right pixel");
  assert_eq!(
    (
      dark_right.red(),
      dark_right.green(),
      dark_right.blue(),
      dark_right.alpha()
    ),
    (232, 232, 232, 255),
    "expected CanvasText in dark scheme"
  );
}
