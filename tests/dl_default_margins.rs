use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_with_backend(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>\
      html, body { margin: 0; background: rgb(0, 255, 0); font-size: 16px; }\
      dl { background: rgb(255, 0, 0); }\
    </style>\
    <dl><dt>Term</dt><dd>Definition</dd></dl>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 64, 64).expect("render")
}

fn assert_dl_margins(pixmap: &tiny_skia::Pixmap) {
  // UA default margins for <dl> should match other list-like blocks: 1em above/below.
  // We set the root font-size to 16px, so `1em` should be 16px.
  let before = pixmap.pixel(1, 8).expect("sample pixel before dl");
  assert!(
    before.green() > 200 && before.red() < 80 && before.blue() < 80,
    "expected dl top margin to show green body background, got rgba=({}, {}, {}, {})",
    before.red(),
    before.green(),
    before.blue(),
    before.alpha()
  );

  let inside = pixmap.pixel(1, 20).expect("sample pixel inside dl");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected dl background to start after 1em margin, got rgba=({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn display_list_dl_has_default_margins() {
  let pixmap = render_with_backend("display_list");
  assert_dl_margins(&pixmap);
}

#[test]
fn legacy_dl_has_default_margins() {
  let pixmap = render_with_backend("legacy");
  assert_dl_margins(&pixmap);
}

