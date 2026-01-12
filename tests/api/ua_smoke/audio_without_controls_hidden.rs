use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_audio_without_controls(backend: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Per HTML UA styles, audio elements without `controls` should not be rendered by default.
  // Many real pages include `<audio>` tags for tracking/autoplay but rely on them being invisible.
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,200,0);}</style>\
    <audio src='' style='width:40px;height:40px;background:rgb(255,0,0);'></audio>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(html, 80, 80).expect("render audio")
}

fn assert_audio_hidden(pixmap: &tiny_skia::Pixmap) {
  // If the audio element were painted (or its placeholder), we'd see non-green pixels near the
  // origin. The UA rule should keep it display:none, leaving only the page background.
  let px = pixmap.pixel(5, 5).expect("sample pixel");
  assert!(
    px.green() > 150 && px.red() < 100 && px.blue() < 100,
    "expected audio without controls to be hidden (got rgba=({}, {}, {}, {}))",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}

#[test]
fn display_list_audio_without_controls_is_hidden() {
  let pixmap = render_audio_without_controls("display_list");
  assert_audio_hidden(&pixmap);
}

#[test]
fn legacy_audio_without_controls_is_hidden() {
  let pixmap = render_audio_without_controls("legacy");
  assert_audio_hidden(&pixmap);
}
