use crate::geometry::Point;
use crate::scroll::{build_scroll_chain, resolve_effective_scroll_state_for_paint, ScrollState};
use crate::{FastRender, PreparedPaintOptions, RenderOptions};

#[test]
fn effective_scroll_state_resolves_viewport_scroll_snap() {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          html { scroll-snap-type: y mandatory; }
          .snap { height: 100px; scroll-snap-align: start; }
        </style>
      </head>
      <body>
        <div class="snap"></div>
        <div class="snap"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(100, 100))
    .expect("prepare");

  let requested = ScrollState::with_viewport(Point::new(0.0, 60.0));
  let scrollport_viewport = prepared.layout_viewport();
  let effective = resolve_effective_scroll_state_for_paint(
    prepared.fragment_tree(),
    requested.clone(),
    scrollport_viewport,
  );

  assert!(
    (effective.viewport.y - 100.0).abs() < 1.0,
    "expected scroll snap to land at 100px, got {:?}",
    effective.viewport
  );

  let frame = prepared
    .paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(requested),
      viewport: None,
      background: None,
      animation_time: None,
      media_provider: None,
    })
    .expect("paint");
  assert_eq!(
    frame.scroll_state, effective,
    "helper should resolve the same scroll state as paint"
  );
}

#[test]
fn effective_scroll_state_clamps_viewport_scroll_to_root_bounds() {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          .content { height: 300px; background: red; }
        </style>
      </head>
      <body>
        <div class="content"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(100, 100))
    .expect("prepare");

  let scrollport_viewport = prepared.layout_viewport();
  let requested = ScrollState::with_viewport(Point::new(0.0, 10_000.0));
  let effective = resolve_effective_scroll_state_for_paint(
    prepared.fragment_tree(),
    requested.clone(),
    scrollport_viewport,
  );

  let mut tree = prepared.fragment_tree().clone();
  tree.ensure_scroll_metadata();
  let bounds = build_scroll_chain(&tree.root, scrollport_viewport, &[])
    .first()
    .expect("root scroll chain")
    .bounds;

  assert!(
    (effective.viewport.y - bounds.max_y).abs() < 1e-3,
    "expected viewport scroll to clamp to max_y={}, got {:?}",
    bounds.max_y,
    effective.viewport
  );

  let frame = prepared
    .paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(requested),
      viewport: None,
      background: None,
      animation_time: None,
      media_provider: None,
    })
    .expect("paint");
  assert_eq!(
    frame.scroll_state, effective,
    "helper should resolve the same scroll state as paint"
  );
}
