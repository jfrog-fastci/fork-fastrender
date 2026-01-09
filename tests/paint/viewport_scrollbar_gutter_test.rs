use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn viewport_reserves_scrollbar_gutter_for_body_overflow_auto() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Headless Chrome reserves space for the viewport scrollbar when the document overflows (and when
  // `overflow: auto` is in effect), but it does not paint the native scrollbar into screenshots.
  // The reserved gutter therefore shows the canvas background, typically the `<html>` background.
  //
  // Without reserving the gutter, the body would paint across the full viewport width and hide the
  // root background, causing diffs on pages where `html`/`body` backgrounds differ (e.g. nasa.gov).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: rgb(0, 0, 255); }
      body { background: rgb(255, 0, 0); overflow-y: auto; }
      .tall { height: 200px; }
    </style>
    <div class="tall"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
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

  let gutter = pixmap.pixel(99, 50).expect("gutter pixel");
  assert!(
    gutter.blue() > 200 && gutter.red() < 80 && gutter.green() < 80,
    "expected canvas background (blue) in the scrollbar gutter, got rgba({}, {}, {}, {})",
    gutter.red(),
    gutter.green(),
    gutter.blue(),
    gutter.alpha()
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

  // When classic scrollbars reserve a gutter, `position: fixed` elements are still sized relative
  // to the full visual viewport (including the gutter). This matches Chrome's behavior and is
  // required for fixed headers to span the scrollbar gutter.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: rgb(0, 0, 255); }
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
