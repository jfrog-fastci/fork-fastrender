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
      figure { background: rgb(255, 0, 0); height: 20px; }\
    </style>\
    <figure></figure>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 128, 64).expect("render")
}

fn assert_figure_margins(pixmap: &tiny_skia::Pixmap) {
  // Chrome's UA stylesheet gives <figure> a block margin similar to <blockquote>:
  // - 1em above/below (16px with the font-size we set)
  // - 40px on left/right.
  let top_margin = pixmap.pixel(50, 8).expect("top margin sample");
  assert!(
    top_margin.green() > 200 && top_margin.red() < 80 && top_margin.blue() < 80,
    "expected top margin to show green background, got rgba=({}, {}, {}, {})",
    top_margin.red(),
    top_margin.green(),
    top_margin.blue(),
    top_margin.alpha()
  );

  let left_margin = pixmap.pixel(20, 20).expect("left margin sample");
  assert!(
    left_margin.green() > 200 && left_margin.red() < 80 && left_margin.blue() < 80,
    "expected left margin to show green background, got rgba=({}, {}, {}, {})",
    left_margin.red(),
    left_margin.green(),
    left_margin.blue(),
    left_margin.alpha()
  );

  let inside = pixmap.pixel(60, 20).expect("inside figure sample");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected figure background to start after default margins, got rgba=({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn display_list_figure_has_default_margins() {
  let pixmap = render_with_backend("display_list");
  assert_figure_margins(&pixmap);
}

#[test]
fn legacy_figure_has_default_margins() {
  let pixmap = render_with_backend("legacy");
  assert_figure_margins(&pixmap);
}
