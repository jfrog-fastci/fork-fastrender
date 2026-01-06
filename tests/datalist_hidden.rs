use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_datalist(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Datalists participate in form controls but are not rendered by default.
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,200,0);}</style>\
    <datalist style='width:40px;height:40px;background:rgb(255,0,0);'></datalist>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 80, 80).expect("render datalist")
}

fn assert_hidden(pixmap: &tiny_skia::Pixmap) {
  let px = pixmap.pixel(5, 5).expect("sample pixel");
  assert!(
    px.green() > 150 && px.red() < 100 && px.blue() < 100,
    "expected element to be hidden (got rgba=({}, {}, {}, {}))",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}

#[test]
fn display_list_datalist_is_hidden_by_default() {
  let pixmap = render_datalist("display_list");
  assert_hidden(&pixmap);
}

#[test]
fn legacy_datalist_is_hidden_by_default() {
  let pixmap = render_datalist("legacy");
  assert_hidden(&pixmap);
}
