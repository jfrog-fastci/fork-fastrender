use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_with_backend(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Browsers center table captions by default. Ensure the UA stylesheet applies `text-align: center`
  // so inline contents inside a <caption> are centered.
  let html = "<!doctype html>\
    <style>\
      html, body { margin: 0; background: rgb(0, 255, 0); }\
      table { border-collapse: collapse; border-spacing: 0; width: 100px; }\
      caption { width: 100px; height: 20px; background: rgb(255, 0, 0); }\
      caption .marker { display: inline-block; width: 10px; height: 10px; background: rgb(0, 0, 255); }\
      td { width: 100px; height: 10px; }\
    </style>\
    <table>\
      <caption><span class='marker'></span></caption>\
      <tr><td></td></tr>\
    </table>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 120, 40).expect("render")
}

fn assert_caption_centers_inline_content(pixmap: &tiny_skia::Pixmap) {
  // Sample within the caption background near the left edge. If the marker is correctly centered,
  // this should still be red.
  let left = pixmap.pixel(5, 5).expect("left sample");
  assert!(
    left.red() > 200 && left.green() < 80 && left.blue() < 80,
    "expected caption background (red) near left edge, got rgba=({}, {}, {}, {})",
    left.red(),
    left.green(),
    left.blue(),
    left.alpha()
  );

  // Sample near the horizontal center of the caption: the blue marker should be centered there.
  let center = pixmap.pixel(50, 5).expect("center sample");
  assert!(
    center.blue() > 200 && center.red() < 80 && center.green() < 80,
    "expected centered marker (blue) near caption center, got rgba=({}, {}, {}, {})",
    center.red(),
    center.green(),
    center.blue(),
    center.alpha()
  );
}

#[test]
fn display_list_caption_centers_inline_content_by_default() {
  let pixmap = render_with_backend("display_list");
  assert_caption_centers_inline_content(&pixmap);
}

#[test]
fn legacy_caption_centers_inline_content_by_default() {
  let pixmap = render_with_backend("legacy");
  assert_caption_centers_inline_content(&pixmap);
}
