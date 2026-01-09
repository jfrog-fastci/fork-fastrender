use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn count_reddish(pixmap: &Pixmap) -> usize {
  let mut total = 0usize;
  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      // Placeholder text is painted with ~60% alpha; once composited over an opaque background
      // it can land well below 255 in the red channel. Keep the threshold generous.
      if px.red() > 80 && px.green() < 60 && px.blue() < 60 {
        total += 1;
      }
    }
  }
  total
}

fn render_input_with_backend(backend: &str) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Value is a single space. Placeholder must *not* render (only an empty string triggers it).
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:#000}</style>\
    <input placeholder=\"X\" value=\" \" style=\"display:block;margin:0;width:120px;height:80px;background:#000;border:0;padding:0;color:rgb(255,0,0);font-size:60px;line-height:1;\">";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer
    .render_html(html, 160, 120)
    .expect("render whitespace placeholder case")
}

#[test]
fn display_list_placeholder_not_shown_for_whitespace_value() {
  let pixmap = render_input_with_backend("display_list");
  let reddish = count_reddish(&pixmap);
  assert_eq!(
    reddish, 0,
    "expected no placeholder pixels when value contains whitespace (reddish={reddish})"
  );
}

#[test]
fn legacy_placeholder_not_shown_for_whitespace_value() {
  let pixmap = render_input_with_backend("legacy");
  let reddish = count_reddish(&pixmap);
  assert_eq!(
    reddish, 0,
    "expected no placeholder pixels when value contains whitespace (reddish={reddish})"
  );
}

