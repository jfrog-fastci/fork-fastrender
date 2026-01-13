use crate::debug::runtime::RuntimeToggles;
use crate::paint::display_list::{DisplayItem, DisplayList, FillRectItem};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::text::font_db::FontConfig;
use crate::text::font_loader::FontContext;
use crate::{
  FastRender, LayoutParallelism, PaintParallelism, Point, Rect, RenderArtifactRequest,
  RenderArtifacts, RenderOptions, Rgba,
};
use std::collections::HashMap;

fn striped_list(width: f32, stripe_h: f32, stripes: usize, scroll_y: f32) -> DisplayList {
  let mut list = DisplayList::with_capacity(stripes);
  for idx in 0..stripes {
    let y = idx as f32 * stripe_h - scroll_y;
    let color = if idx % 2 == 0 { Rgba::RED } else { Rgba::BLUE };
    list.push(DisplayItem::FillRect(FillRectItem {
      rect: Rect::from_xywh(0.0, y, width, stripe_h),
      color,
    }));
  }
  list
}

#[test]
fn scroll_blit_matches_full_paint_dpr2_integer_device_shift() {
  // DPR=2 and a 0.5 CSS px scroll delta produces an exact 1-device-pixel shift. This catches
  // off-by-one errors when converting between CSS and device pixels (especially for the exposed
  // stripe repaint region).
  let width_css = 16u32;
  let height_css = 16u32;
  let scale = 2.0;
  let scroll_a = 0.0;
  let scroll_b = 0.5;
  let delta = scroll_b - scroll_a;

  let stripe_h = 0.5; // => 1 device px tall at DPR=2
  let stripes = ((height_css as f32 + scroll_b + 2.0) / stripe_h).ceil() as usize;
  let list_a = striped_list(width_css as f32, stripe_h, stripes, scroll_a);
  let list_b = striped_list(width_css as f32, stripe_h, stripes, scroll_b);

  let full_a = DisplayListRenderer::new_scaled(
    width_css,
    height_css,
    Rgba::WHITE,
    FontContext::new(),
    scale,
  )
  .expect("renderer")
  .with_parallelism(PaintParallelism::disabled())
  .render(&list_a)
  .expect("full paint A");

  let optimized_report = DisplayListRenderer::new_scaled_from_existing_pixmap(
    full_a,
    Rgba::WHITE,
    FontContext::new(),
    scale,
  )
  .expect("renderer")
  .with_parallelism(PaintParallelism::disabled())
  .render_scroll_blit_with_report(&list_b, Point::new(0.0, delta))
  .expect("scroll blit paint B");

  assert!(
    optimized_report.scroll_blit_used,
    "expected scroll blit to be used, got fallback={:?}",
    optimized_report.fallback_reason
  );
  assert!(
    optimized_report.partial_repaint_used,
    "expected scroll blit to repaint the exposed stripe"
  );
  assert!(
    optimized_report.fallback_reason.is_none(),
    "unexpected scroll blit fallback: {:?}",
    optimized_report.fallback_reason
  );

  let full_b = DisplayListRenderer::new_scaled(
    width_css,
    height_css,
    Rgba::WHITE,
    FontContext::new(),
    scale,
  )
  .expect("renderer")
  .with_parallelism(PaintParallelism::disabled())
  .render(&list_b)
  .expect("full paint B");

  assert_eq!(
    optimized_report.pixmap.data(),
    full_b.data(),
    "optimized (scroll-blit) output should match full repaint at DPR=2"
  );
}

fn capture_display_list(
  renderer: &mut FastRender,
  html: &str,
  options: RenderOptions,
) -> DisplayList {
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts
    .display_list
    .take()
    .expect("display list artifact should be present")
}

#[test]
fn scroll_blit_matches_full_paint_with_scrollbar_gutter_viewport_inset() {
  // Regression test for scroll blit + stripe repaint when the *layout* viewport is inset within
  // the paint viewport (e.g. `scrollbar-gutter: stable both-edges`). The exposed stripe must be
  // repainted across the full paint surface (including the gutter region), and scroll deltas must
  // still map to correct device-pixel shifts.
  let viewport_w = 100u32;
  let viewport_h = 100u32;
  let scroll_a = 0.0;
  let scroll_b = 3.0;
  let delta = scroll_b - scroll_a;

  let html = r#"<!doctype html>
    <style>
      html { background: transparent; scrollbar-gutter: stable both-edges; }
      body {
        margin: 0;
        overflow-y: auto;
        /* Y-varying background to make stale gutter pixels obvious. */
        background: repeating-linear-gradient(
          to bottom,
          rgb(255, 0, 0) 0px,
          rgb(255, 0, 0) 1px,
          rgb(0, 0, 255) 1px,
          rgb(0, 0, 255) 2px
        );
      }
      #marker { width: 100%; height: 20px; background: rgb(0, 255, 0); }
      .tall { height: 300px; }
    </style>
    <div id="marker"></div>
    <div class="tall"></div>
  "#;

  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
    // Ensure classic scrollbar gutters are reserved even if the test runner sets
    // FASTR_HIDE_SCROLLBARS=1 globally.
    ("FASTR_HIDE_SCROLLBARS".to_string(), "0".to_string()),
  ]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let options_for_scroll = |scroll_y: f32, delta_y: f32| {
    RenderOptions::new()
      .with_viewport(viewport_w, viewport_h)
      .with_runtime_toggles(toggles.clone())
      .with_paint_parallelism(PaintParallelism::disabled())
      .with_layout_parallelism(LayoutParallelism::disabled())
      .with_scroll(0.0, scroll_y)
      .with_scroll_delta(0.0, delta_y)
  };

  let list_a = capture_display_list(&mut renderer, html, options_for_scroll(scroll_a, 0.0));
  let list_b = capture_display_list(&mut renderer, html, options_for_scroll(scroll_b, delta));

  let background = Rgba::rgb(255, 0, 255); // magenta: should never leak if stripe repaint is correct
  let full_a = DisplayListRenderer::new(viewport_w, viewport_h, background, FontContext::new())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list_a)
    .expect("full paint A");

  // Sanity-check that `scrollbar-gutter: stable both-edges` actually produced a non-zero inset by
  // asserting that the scrollport content (`#marker`) does not cover the leftmost pixel.
  let gutter_px = full_a.pixel(0, 5).expect("gutter pixel");
  assert!(
    gutter_px.green() < 80,
    "expected left gutter to not be painted by the green marker; got rgba({}, {}, {}, {})",
    gutter_px.red(),
    gutter_px.green(),
    gutter_px.blue(),
    gutter_px.alpha()
  );
  let inside_px = full_a.pixel(20, 5).expect("inside pixel");
  assert!(
    inside_px.green() > 200 && inside_px.red() < 80 && inside_px.blue() < 80,
    "expected scrollport content marker (green) inside the inset viewport; got rgba({}, {}, {}, {})",
    inside_px.red(),
    inside_px.green(),
    inside_px.blue(),
    inside_px.alpha()
  );

  let optimized_report =
    DisplayListRenderer::new_from_existing_pixmap(full_a, background, FontContext::new())
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render_scroll_blit_with_report(&list_b, Point::new(0.0, delta))
      .expect("scroll blit paint B");

  assert!(
    optimized_report.scroll_blit_used,
    "expected scroll blit to be used, got fallback={:?}",
    optimized_report.fallback_reason
  );
  assert!(
    optimized_report.partial_repaint_used,
    "expected scroll blit to repaint the exposed stripe"
  );
  assert!(
    optimized_report.fallback_reason.is_none(),
    "unexpected scroll blit fallback: {:?}",
    optimized_report.fallback_reason
  );

  let full_b = DisplayListRenderer::new(viewport_w, viewport_h, background, FontContext::new())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list_b)
    .expect("full paint B");

  assert_eq!(
    optimized_report.pixmap.data(),
    full_b.data(),
    "optimized (scroll-blit) output should match full repaint with viewport inset (scrollbar-gutter)"
  );
}
