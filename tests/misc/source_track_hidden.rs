use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_with_backend(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // <source> and <track> are non-rendered elements in browser UAs. Ensure we treat them as
  // display:none by default so their padding/background does not affect layout/paint.
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,255,0);}</style>\
    <source style='padding:10px;background:rgb(255,0,0);'>\
    <track style='padding:10px;background:rgb(255,0,0);'>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 64, 64).expect("render")
}

fn assert_hidden(pixmap: &tiny_skia::Pixmap) {
  let px = pixmap.pixel(5, 5).expect("sample pixel");
  assert!(
    px.green() > 200 && px.red() < 80 && px.blue() < 80,
    "expected background to remain visible (got rgba=({}, {}, {}, {}))",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}

#[test]
fn display_list_source_track_are_hidden_by_default() {
  let pixmap = render_with_backend("display_list");
  assert_hidden(&pixmap);
}

#[test]
fn legacy_source_track_are_hidden_by_default() {
  let pixmap = render_with_backend("legacy");
  assert_hidden(&pixmap);
}

