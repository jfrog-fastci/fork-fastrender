//! Benchmark scroll repaint performance.
//!
//! Measures the per-step cost of:
//! - Baseline: full `PreparedDocument::paint_with_options_frame` at successive scroll offsets.
//! - Optimized: "scroll blit" (memmove existing pixels) + repaint only the newly exposed stripe,
//!   with an overlap band to account for effects like box-shadow near the viewport edge.
//!
//! Run with:
//! ```bash
//! cargo bench --bench scroll_blit_bench
//! ```

use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use criterion::Criterion;
use fastrender::scroll::build_scroll_chain;
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, PreparedDocument, PreparedPaintOptions, Rect, RenderOptions, Size};
use fastrender::{Pixmap, Result};
use fastrender::paint::display_list_renderer::PaintParallelism;
use std::time::Instant;

mod common;

const VIEWPORT_W: u32 = 800;
const VIEWPORT_H: u32 = 600;
const SCROLL_DY_PX: u32 = 10;
const STEPS_PER_SAMPLE: u32 = 100;

/// Extra rows repainted above the newly exposed stripe to avoid artifacts from effects like
/// box-shadow/blur that bleed into the visible area.
///
/// This benchmark's HTML intentionally places a box-shadow flush with the initial viewport bottom
/// edge; without an overlap band the first scroll step would need to repaint pixels above the
/// stripe to include the shadow halo.
const STRIPE_OVERLAP_PX: u32 = 64;

fn build_scroll_fixture_html() -> String {
  // A single tall block with a deterministic repeating pattern (to keep DOM small and repaint
  // bounded), plus a box-shadow element placed flush with the initial viewport bottom edge.
  //
  // The shadow bleeds below the viewport at scroll_y=0 and becomes visible in the newly exposed
  // stripe after the first scroll step, exercising overlap repaint logic.
  format!(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{ margin: 0; padding: 0; }}
      #spacer {{
        position: relative;
        height: 500000px;
        background: repeating-linear-gradient(
          to bottom,
          rgb(246, 246, 246) 0px,
          rgb(246, 246, 246) 20px,
          rgb(232, 240, 255) 20px,
          rgb(232, 240, 255) 40px
        );
      }}
      #shadow {{
        position: absolute;
        left: 32px;
        top: {shadow_top}px;
        width: 320px;
        height: 40px;
        background: white;
        border-radius: 8px;
        box-shadow: 0 0 32px rgba(0, 0, 0, 0.35);
      }}
    </style>
  </head>
  <body>
    <div id="spacer">
      <div id="shadow"></div>
    </div>
  </body>
</html>"#,
    shadow_top = VIEWPORT_H.saturating_sub(40)
  )
}

fn max_scroll_y(prepared: &PreparedDocument) -> f32 {
  let viewport = Size::new(VIEWPORT_W as f32, VIEWPORT_H as f32);
  build_scroll_chain(&prepared.fragment_tree().root, viewport, &[])
    .first()
    .map(|state| state.bounds.max_y)
    .filter(|v| v.is_finite() && *v > 0.0)
    .unwrap_or(0.0)
}

fn warm_initial_frame(prepared: &PreparedDocument) -> Result<Pixmap> {
  Ok(
    prepared
      .paint_with_options_frame(PreparedPaintOptions::default())?
      .pixmap,
  )
}

fn blit_scroll_up_in_place(pixmap: &mut Pixmap, dy_px: u32) {
  let dy = dy_px as usize;
  if dy == 0 {
    return;
  }
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;
  if dy >= height {
    return;
  }
  let bytes_per_row = width * 4;
  let shift = dy * bytes_per_row;
  let total = height * bytes_per_row;
  pixmap.data_mut().copy_within(shift..total, 0);
}

fn repaint_bottom_band(
  prepared: &PreparedDocument,
  pixmap: &mut Pixmap,
  old_scroll_y: f32,
  dy_px: u32,
  overlap_px: u32,
) -> Result<()> {
  let dy = dy_px as usize;
  let overlap = overlap_px as usize;
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;
  let bytes_per_row = width * 4;

  let repaint_rows = dy.saturating_add(overlap).min(height);
  if repaint_rows == 0 {
    return Ok(());
  }

  // After scrolling down by `dy_px`, the viewport shows:
  //   [old_scroll_y + dy_px, old_scroll_y + dy_px + viewport_h]
  // The bottom band of height `dy+overlap` in *viewport* space corresponds to this page rect:
  //   [old_scroll_y + viewport_h - overlap, old_scroll_y + viewport_h + dy]
  let repaint_y = (old_scroll_y + height as f32 - overlap_px as f32).max(0.0);
  let region = Rect::from_xywh(0.0, repaint_y, width as f32, repaint_rows as f32);
  let region_pixmap = prepared.paint_region(region)?;

  let copy_bytes = repaint_rows * bytes_per_row;
  let dst_start_row = height - repaint_rows;
  let dst_start = dst_start_row * bytes_per_row;

  // Overwrite the destination band with the repainted pixels (includes overlap).
  pixmap.data_mut()[dst_start..dst_start + copy_bytes]
    .copy_from_slice(&region_pixmap.data()[..copy_bytes]);

  Ok(())
}

fn scroll_blit_stripe_step(
  prepared: &PreparedDocument,
  pixmap: &mut Pixmap,
  scroll_y: &mut f32,
  max_scroll_y: f32,
) -> Result<()> {
  let dy = SCROLL_DY_PX as f32;
  if *scroll_y + dy > max_scroll_y && max_scroll_y > 0.0 {
    // Wrap back to the top to avoid clamping (which would turn scroll into a no-op).
    *scroll_y = 0.0;
    *pixmap = warm_initial_frame(prepared)?;
    return Ok(());
  }

  let old_scroll_y = *scroll_y;
  *scroll_y += dy;

  blit_scroll_up_in_place(pixmap, SCROLL_DY_PX);
  repaint_bottom_band(prepared, pixmap, old_scroll_y, SCROLL_DY_PX, STRIPE_OVERLAP_PX)?;

  Ok(())
}

fn scroll_full_paint_step(
  prepared: &PreparedDocument,
  scroll_y: &mut f32,
  max_scroll_y: f32,
) -> Result<Pixmap> {
  let dy = SCROLL_DY_PX as f32;
  if *scroll_y + dy > max_scroll_y && max_scroll_y > 0.0 {
    *scroll_y = 0.0;
  } else {
    *scroll_y += dy;
  }

  Ok(
    prepared
      .paint_with_options_frame(PreparedPaintOptions::default().with_scroll(0.0, *scroll_y))?
      .pixmap,
  )
}

fn bench_scroll_repaint(c: &mut Criterion) {
  common::bench_print_config_once("scroll_blit_bench", &[]);

  let html = build_scroll_fixture_html();
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let options = RenderOptions::new()
    .with_viewport(VIEWPORT_W, VIEWPORT_H)
    .with_device_pixel_ratio(1.0)
    .with_paint_parallelism(PaintParallelism::disabled());

  let prepared = renderer
    .prepare_html(&html, options)
    .expect("prepare html");

  let max_scroll_y = max_scroll_y(&prepared);

  // Warm caches with an initial full paint.
  let warm = warm_initial_frame(&prepared).expect("warm paint");
  black_box(warm.data());

  let mut group = c.benchmark_group("scroll_repaint");
  group.sample_size(10);

  group.bench_function("full_paint_per_step", |b| {
    b.iter_custom(|iters| {
      let mut scroll_y = 0.0f32;
      // Warm up this sample: ensure caches are initialized and the first scroll step is not a
      // "first paint" outlier.
      let _ = warm_initial_frame(&prepared).expect("warm");

      let start = Instant::now();
      for _ in 0..iters {
        for _ in 0..STEPS_PER_SAMPLE {
          let pixmap = scroll_full_paint_step(&prepared, &mut scroll_y, max_scroll_y).expect("paint");
          black_box(pixmap.data());
        }
      }
      // Report time per scroll step (criterion divides by `iters`).
      start.elapsed() / STEPS_PER_SAMPLE
    });
  });

  group.bench_function("scroll_blit_plus_stripe_repaint_per_step", |b| {
    b.iter_custom(|iters| {
      let mut scroll_y = 0.0f32;
      let mut pixmap = warm_initial_frame(&prepared).expect("warm");

      let start = Instant::now();
      for _ in 0..iters {
        for _ in 0..STEPS_PER_SAMPLE {
          scroll_blit_stripe_step(&prepared, &mut pixmap, &mut scroll_y, max_scroll_y)
            .expect("step");
          black_box(pixmap.data());
        }
      }
      start.elapsed() / STEPS_PER_SAMPLE
    });
  });

  group.finish();
}

criterion_group!(
  name = scroll_blit_benches;
  config = common::perf_criterion();
  targets = bench_scroll_repaint
);
criterion_main!(scroll_blit_benches);
