use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn viewport_does_not_reserve_scrollbar_gutter_for_body_overflow_auto() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Chrome fixture baselines are captured with `--hide-scrollbars`, and modern headless Chrome uses
  // overlay scrollbars in that configuration. The viewport scrollbar therefore does not reserve any
  // layout space when the document overflows.
  let html = r#"<!doctype html>
    <style>
      html { background: rgb(0, 0, 255); }
      body { margin: 0; overflow-y: auto; background: transparent; }
      #marker { background: rgb(255, 0, 0); height: 50px; width: 100%; }
      .tall { height: 200px; }
    </style>
    <div id="marker"></div>
    <div class="tall"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  let inside = pixmap.pixel(99, 25).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected marker background (red) to extend to the right edge (no viewport scrollbar gutter), got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn viewport_fixed_elements_paint_into_scrollbar_gutter() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // When classic scrollbars reserve a gutter (e.g. via `scrollbar-gutter: stable`), `position:
  // fixed` elements are still sized relative to the full visual viewport (including the gutter).
  // This matches Chrome's behavior and is required for fixed headers to span the scrollbar gutter.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: rgb(0, 0, 255); scrollbar-gutter: stable; }
      body { background: rgb(255, 0, 0); overflow-y: auto; }
      .banner {
        position: fixed;
        top: 0;
        left: 0;
        width: 100%;
        height: 10px;
        background: rgb(0, 255, 0);
      }
      .tall { height: 200px; }
    </style>
    <div class="banner"></div>
    <div class="tall"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  let inside = pixmap.pixel(10, 5).expect("inside pixel");
  assert!(
    inside.green() > 200 && inside.red() < 80 && inside.blue() < 80,
    "expected fixed banner background (green) inside the viewport, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let gutter = pixmap.pixel(99, 5).expect("gutter pixel");
  assert!(
    gutter.green() > 200 && gutter.red() < 80 && gutter.blue() < 80,
    "expected fixed banner to paint into scrollbar gutter, got rgba({}, {}, {}, {})",
    gutter.red(),
    gutter.green(),
    gutter.blue(),
    gutter.alpha()
  );
}

#[test]
fn viewport_scrollbar_gutter_preserves_root_border() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // When the `<body>` background is promoted to the canvas (HTML canvas background propagation),
  // the body's border should still appear in the scrollbar gutter region (which is part of the
  // painted canvas, even though layout is performed with a reduced viewport).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: transparent; scrollbar-gutter: stable; }
      body {
        background: rgb(255, 0, 0);
        border-top: 1px solid rgb(0, 255, 0);
        overflow-y: auto;
      }
      .tall { height: 200px; }
    </style>
    <div class="tall"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  let border = pixmap.pixel(99, 0).expect("border pixel");
  assert!(
    border.green() > 200 && border.red() < 80 && border.blue() < 80,
    "expected propagated body border (green) in the scrollbar gutter, got rgba({}, {}, {}, {})",
    border.red(),
    border.green(),
    border.blue(),
    border.alpha()
  );

  let gutter = pixmap.pixel(99, 50).expect("gutter pixel");
  assert!(
    gutter.red() > 200 && gutter.green() < 80 && gutter.blue() < 80,
    "expected canvas background (red) in the scrollbar gutter, got rgba({}, {}, {}, {})",
    gutter.red(),
    gutter.green(),
    gutter.blue(),
    gutter.alpha()
  );
}
