use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, FastRenderConfig};
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

#[test]
fn canvas_background_propagation_ignores_negative_paint_bounds_for_background_positioning() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // The propagated canvas background's positioning area should be based on the document canvas
  // (viewport/scrollable overflow), not conservative paint bounds for the root stacking context.
  //
  // Positioned descendants can extend far into negative coordinates (e.g. decorative offscreen
  // elements). Those should not skew `background-position` / gradient interpolation for the canvas
  // background.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      body {
        background: linear-gradient(to right, rgb(255, 0, 0), rgb(0, 0, 255));
        height: 50px;
      }
      .offscreen {
        position: absolute;
        left: -1000px;
        top: 0;
        width: 1px;
        height: 1px;
        background: rgb(0, 255, 0);
      }
    </style>
    <div class="offscreen"></div>
    <div style="height: 50px"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 200, 100)
    .expect("render should succeed");

  // Sample below the body border box: the left edge should still be near the gradient start (red).
  let outside_left = pixmap.pixel(1, 75).expect("outside-left pixel");
  assert!(
    outside_left.red() > 200 && outside_left.green() < 80 && outside_left.blue() < 80,
    "expected propagated canvas background to start at red despite negative paint bounds, got rgba({}, {}, {}, {})",
    outside_left.red(),
    outside_left.green(),
    outside_left.blue(),
    outside_left.alpha()
  );
}

#[test]
fn canvas_background_propagates_body_background_with_positioned_body_and_html_whitespace() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // HTML parsing inserts inter-element whitespace between `</head>` and `<body>` as a direct child
  // of `<html>`. That whitespace should not generate boxes: otherwise the `<body>` box id is no
  // longer `html_id + 1` and canvas background propagation can fail.
  //
  // When propagation fails, the body background is painted as a positioned element (above negative
  // z-index descendants) instead of as the canvas background (below everything).
  let html = r#"<!doctype html><html><head><style>
      html, body { margin: 0; }
      html { background: transparent; }
      body { position: absolute; top: 0; left: 0; right: 0; height: 100px; background: rgb(255, 0, 0); }
      #neg { position: absolute; left: 0; top: 0; width: 40px; height: 40px; background: rgb(0, 0, 255); z-index: -1; }
    </style></head>
    <body><div id="neg"></div></body></html>"#;

  let pixmap = renderer
    .render_html(html, 64, 200)
    .expect("render should succeed");

  let neg = pixmap.pixel(10, 10).expect("pixel inside #neg");
  assert!(
    neg.blue() > 200 && neg.red() < 80 && neg.green() < 80,
    "expected negative z-index content (blue) to paint above propagated canvas background, got rgba({}, {}, {}, {})",
    neg.red(),
    neg.green(),
    neg.blue(),
    neg.alpha()
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
