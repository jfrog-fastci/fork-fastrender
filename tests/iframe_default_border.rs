use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

fn render_iframe(backend: &str, frameborder: Option<&str>) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let inner = "<!doctype html><style>html,body{margin:0;background:rgb(255,0,0);}</style>";
  let frameborder_attr = frameborder
    .map(|value| format!(" frameborder=\"{value}\""))
    .unwrap_or_default();
  let html = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,255,0);}}</style>\
     <iframe{frameborder_attr} srcdoc='{inner}' style='display:block;margin:0;width:20px;height:20px;'></iframe>"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer.render_html(&html, 32, 32).expect("render iframe")
}

fn assert_default_iframe_border(pixmap: &tiny_skia::Pixmap) {
  // Default UA styles should give iframes a visible 2px inset border. Sample inside the left
  // border (x=1) and ensure it's gray (not the red iframe content and not the green page bg).
  let border = pixmap.pixel(1, 10).expect("border pixel");
  assert!(
    border.red().abs_diff(border.green()) <= 5 && border.green().abs_diff(border.blue()) <= 5,
    "expected border to be gray, got rgba=({}, {}, {}, {})",
    border.red(),
    border.green(),
    border.blue(),
    border.alpha()
  );
  assert!(
    border.red() > 100 && border.red() < 210,
    "expected border gray value to be in-range, got rgba=({}, {}, {}, {})",
    border.red(),
    border.green(),
    border.blue(),
    border.alpha()
  );

  let inside = pixmap.pixel(4, 4).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected iframe content to be red, got rgba=({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

fn assert_frameborder_zero_removes_border(pixmap: &tiny_skia::Pixmap) {
  // With frameborder=0 the border should be suppressed, so the red iframe content should start
  // at the origin.
  let px = pixmap.pixel(1, 10).expect("sample pixel");
  assert!(
    px.red() > 200 && px.green() < 80 && px.blue() < 80,
    "expected iframe content to be red at the edge when frameborder=0, got rgba=({}, {}, {}, {})",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}

#[test]
fn display_list_iframe_has_default_border() {
  let pixmap = render_iframe("display_list", None);
  assert_default_iframe_border(&pixmap);
}

#[test]
fn legacy_iframe_has_default_border() {
  let pixmap = render_iframe("legacy", None);
  assert_default_iframe_border(&pixmap);
}

#[test]
fn display_list_frameborder_zero_removes_border() {
  let pixmap = render_iframe("display_list", Some("0"));
  assert_frameborder_zero_removes_border(&pixmap);
}

#[test]
fn legacy_frameborder_zero_removes_border() {
  let pixmap = render_iframe("legacy", Some("0"));
  assert_frameborder_zero_removes_border(&pixmap);
}

