use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn canvas_background_prefers_root_html_background_over_body() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // In HTML/CSS, the canvas background is taken from the root element when it is non-transparent.
  // The body background should not be propagated over it.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: rgb(0, 0, 255); }
      body { background: rgb(255, 0, 0); height: 100px; }
    </style>
    <div style="height: 100px"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 64, 200)
    .expect("render should succeed");

  let inside = pixmap.pixel(10, 50).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected body background (red) within body bounds, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let outside = pixmap.pixel(10, 150).expect("outside pixel");
  assert!(
    outside.blue() > 200 && outside.red() < 80 && outside.green() < 80,
    "expected canvas background (blue) outside body bounds, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );
}

#[test]
fn canvas_background_propagates_body_background_when_root_is_transparent() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // When the root element background is transparent, HTML canvas background propagation uses the
  // body background and extends it to cover the viewport.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      body { background: rgb(255, 0, 0); height: 100px; }
    </style>
    <div style="height: 100px"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 64, 200)
    .expect("render should succeed");

  let inside = pixmap.pixel(10, 50).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected body background (red) within body bounds, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let outside = pixmap.pixel(10, 150).expect("outside pixel");
  assert!(
    outside.red() > 200 && outside.green() < 80 && outside.blue() < 80,
    "expected propagated canvas background (red) outside body bounds, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );
}

#[test]
fn canvas_background_propagates_body_gradient_background_when_root_is_transparent() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // The propagated canvas background should behave as if the body background were painted on the
  // canvas itself (i.e., sized to the viewport). This is especially observable for gradients, which
  // depend on the box size to determine their color interpolation.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      body {
        background: linear-gradient(to bottom, rgb(255, 0, 0), rgb(0, 0, 255));
        height: 100px;
      }
    </style>
    <div style="height: 100px"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 64, 200)
    .expect("render should succeed");

  // Sample below the body border box: if the gradient is correctly propagated, it should still be
  // visible and skew towards blue (the bottom color) because we're 75% down the viewport.
  let outside = pixmap.pixel(10, 150).expect("outside pixel");
  assert!(
    outside.blue() > 160 && outside.red() < 110 && outside.green() < 80,
    "expected propagated canvas gradient background outside body bounds, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );
}
