use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_canvas_with_backend(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,200,0);}</style>\
    <canvas style='display:block;margin:0;width:20px;height:20px;box-sizing:content-box;\
      border:10px solid rgb(0,0,255);padding:10px;overflow:clip;'></canvas>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 80, 80).expect("render canvas")
}

fn assert_canvas_is_transparent(pixmap: &tiny_skia::Pixmap) {
  // Left border pixel (should be blue).
  let border = pixmap.pixel(5, 30).expect("border pixel");
  assert!(
    border.blue() > 200 && border.red() < 80 && border.green() < 80,
    "expected border to be blue (got rgba=({}, {}, {}, {}))",
    border.red(),
    border.green(),
    border.blue(),
    border.alpha()
  );

  // Content box starts at (border+padding) = (20,20). The canvas has no drawn content, so it
  // should be transparent and show the body background (green) instead of a UA placeholder.
  let inside = pixmap.pixel(25, 25).expect("inside pixel");
  assert!(
    inside.green() > 150 && inside.red() < 100 && inside.blue() < 100,
    "expected canvas interior to show green background (got rgba=({}, {}, {}, {}))",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn display_list_canvas_without_content_is_transparent() {
  let pixmap = render_canvas_with_backend("display_list");
  assert_canvas_is_transparent(&pixmap);
}

#[test]
fn legacy_canvas_without_content_is_transparent() {
  let pixmap = render_canvas_with_backend("legacy");
  assert_canvas_is_transparent(&pixmap);
}
