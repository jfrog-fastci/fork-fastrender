use crate::debug::runtime::RuntimeToggles;
use crate::geometry::{Point, Size};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::text::font_loader::FontContext;
use crate::{FastRender, FastRenderConfig};
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

  let below_banner = pixmap.pixel(99, 50).expect("below-banner pixel");
  assert!(
    below_banner.blue() > 200 && below_banner.red() < 80 && below_banner.green() < 80,
    "expected canvas background (blue) in the stable scrollbar gutter below the fixed banner, got rgba({}, {}, {}, {})",
    below_banner.red(),
    below_banner.green(),
    below_banner.blue(),
    below_banner.alpha()
  );
}

#[test]
fn hide_scrollbars_disables_scrollbar_gutter_stable() {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
    ("FASTR_HIDE_SCROLLBARS".to_string(), "1".to_string()),
  ]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Headless Chrome fixture baselines are captured with `--hide-scrollbars`. In that mode scrollbars
  // reserve no layout space, even if `scrollbar-gutter: stable` is set.
  let html = r#"<!doctype html>
    <style>
      html { background: rgb(0, 0, 255); scrollbar-gutter: stable; }
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

  let edge = pixmap.pixel(99, 25).expect("edge pixel");
  assert!(
    edge.red() > 200 && edge.green() < 80 && edge.blue() < 80,
    "expected marker background (red) to extend to the right edge when scrollbars are hidden, got rgba({}, {}, {}, {})",
    edge.red(),
    edge.green(),
    edge.blue(),
    edge.alpha()
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

#[test]
fn viewport_scrollbar_gutter_both_edges_reserves_left_gutter() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // `scrollbar-gutter: stable both-edges` reserves gutter space on both inline edges when the
  // viewport has classic scrollbars. Layout runs in a reduced scrollport width, but the scrollport
  // is inset so the gutter appears on both the left and right.
  let html = r#"<!doctype html>
    <style>
      html { background: rgb(0, 0, 255); scrollbar-gutter: stable both-edges; }
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

  let left_gutter = pixmap.pixel(0, 25).expect("left gutter pixel");
  assert!(
    left_gutter.blue() > 200 && left_gutter.red() < 80 && left_gutter.green() < 80,
    "expected canvas background (blue) in the left stable scrollbar gutter, got rgba({}, {}, {}, {})",
    left_gutter.red(),
    left_gutter.green(),
    left_gutter.blue(),
    left_gutter.alpha()
  );

  let inside = pixmap.pixel(20, 25).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected marker background (red) inside the scrollport, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let right_gutter = pixmap.pixel(99, 25).expect("right gutter pixel");
  assert!(
    right_gutter.blue() > 200 && right_gutter.red() < 80 && right_gutter.green() < 80,
    "expected canvas background (blue) in the right stable scrollbar gutter, got rgba({}, {}, {}, {})",
    right_gutter.red(),
    right_gutter.green(),
    right_gutter.blue(),
    right_gutter.alpha()
  );
}

#[test]
fn viewport_fixed_elements_paint_into_scrollbar_gutter_both_edges() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Even when the scrollport is inset for `stable both-edges`, viewport-fixed elements are still
  // sized/positioned relative to the full visual viewport (including both gutters).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html { background: rgb(0, 0, 255); scrollbar-gutter: stable both-edges; }
      body { background: transparent; overflow-y: auto; }
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

  let left = pixmap.pixel(0, 5).expect("left pixel");
  assert!(
    left.green() > 200 && left.red() < 80 && left.blue() < 80,
    "expected fixed banner background (green) in the left gutter, got rgba({}, {}, {}, {})",
    left.red(),
    left.green(),
    left.blue(),
    left.alpha()
  );

  let right = pixmap.pixel(99, 5).expect("right pixel");
  assert!(
    right.green() > 200 && right.red() < 80 && right.blue() < 80,
    "expected fixed banner background (green) in the right gutter, got rgba({}, {}, {}, {})",
    right.red(),
    right.green(),
    right.blue(),
    right.alpha()
  );
}

#[test]
fn scroll_blit_disabled_when_viewport_reserves_scrollbar_gutter_space() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // This document reserves classic scrollbar gutters on both axes because `overflow: auto` applies
  // to both inline and block directions. The reserved space shrinks the scrollport while keeping
  // the painted surface at the requested viewport size.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      html {
        background: rgb(0, 0, 0);
        overflow: auto;
        scrollbar-gutter: stable;
      }
      body { background: transparent; }
      .spacer { height: 120px; background: rgb(255, 0, 0); }
      .marker { height: 20px; background: rgb(0, 255, 0); }
    </style>
    <div class="spacer"></div>
    <div class="marker"></div>
    <div class="spacer"></div>
  "#;

  let viewport_w = 100u32;
  let viewport_h = 100u32;
  let scale = 1.0;

  // Render the first frame and capture enough artifacts to detect scrollbar-gutter reservation.
  let mut artifacts = crate::RenderArtifacts::new(crate::RenderArtifactRequest {
    styled_tree: true,
    fragment_tree: true,
    ..crate::RenderArtifactRequest::none()
  });
  let options = crate::RenderOptions::new()
    .with_viewport(viewport_w, viewport_h)
    .with_device_pixel_ratio(scale)
    .with_scroll(0.0, 0.0);
  let pixmap_before = renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("first frame render should succeed");

  let styled_tree = artifacts.styled_tree.as_ref().expect("styled tree");
  let fragment_tree = artifacts.fragment_tree.as_ref().expect("fragment tree");
  let scrollport_viewport = fragment_tree.viewport_size();
  let paint_viewport = Size::new(viewport_w as f32, viewport_h as f32);
  let (gutter_x, gutter_y) = crate::api::viewport_scrollbar_gutter_reserved_axes(
    styled_tree,
    scrollport_viewport,
    paint_viewport,
  );
  assert!(
    gutter_y,
    "expected viewport scrollbar-gutter to reserve horizontal gutter space (scrollport height should be smaller than paint height)"
  );

  // Render the second frame with a vertical scroll delta and capture the display list used for
  // repaint.
  let scroll_delta_y = 10.0;
  let mut artifacts_after = crate::RenderArtifacts::new(crate::RenderArtifactRequest {
    display_list: true,
    ..crate::RenderArtifactRequest::none()
  });
  let options_after = crate::RenderOptions::new()
    .with_viewport(viewport_w, viewport_h)
    .with_device_pixel_ratio(scale)
    .with_scroll(0.0, scroll_delta_y)
    .with_scroll_delta(0.0, scroll_delta_y);
  let pixmap_after_full = renderer
    .render_html_with_options_and_artifacts(html, options_after, &mut artifacts_after)
    .expect("second frame render should succeed");
  let list_after = artifacts_after
    .display_list
    .as_ref()
    .expect("display list should be captured");

  // Attempt the scroll-blit optimization. When the viewport reserves scrollbar gutters on the
  // scroll axis (here: bottom gutter reducing the scrollport height), a full-surface blit is not
  // correct. The renderer should fall back to a full repaint with a clear reason.
  let background = renderer.background_color();
  let font_ctx = FontContext::new();
  let mut blit_renderer = DisplayListRenderer::new_scaled_from_existing_pixmap(
    pixmap_before,
    background,
    font_ctx,
    scale,
  )
  .expect("scroll blit renderer should construct");
  blit_renderer.set_viewport_scrollbar_gutter_reserved_axes(gutter_x, gutter_y);
  let report = blit_renderer
    .render_scroll_blit_with_report(list_after, Point::new(0.0, scroll_delta_y))
    .expect("scroll blit operation should succeed");

  assert!(
    !report.scroll_blit_used,
    "scroll blit must be disabled when viewport scrollbar-gutter reserves space along the scroll axis"
  );
  assert_eq!(
    report.fallback_reason.as_deref(),
    Some("viewport scrollbar-gutter reserved space")
  );
  assert_eq!(report.pixmap.data(), pixmap_after_full.data());
}
