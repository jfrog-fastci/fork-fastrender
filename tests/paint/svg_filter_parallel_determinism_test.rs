use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::{
  BlendMode, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::paint::svg_filter::parse_svg_filter_from_svg_document;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::{Rect, Rgba};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use std::sync::Arc;
use tiny_skia::Pixmap;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;

fn assert_pixmap_eq(serial: &Pixmap, parallel: &Pixmap) {
  assert_eq!(serial.width(), parallel.width(), "pixmap width mismatch");
  assert_eq!(serial.height(), parallel.height(), "pixmap height mismatch");
  let serial_data = serial.data();
  let parallel_data = parallel.data();
  if serial_data == parallel_data {
    return;
  }

  let width = serial.width() as usize;
  let height = serial.height() as usize;
  let mut first_mismatch: Option<(usize, usize, [u8; 4], [u8; 4])> = None;
  let mut diff_min_x = u32::MAX;
  let mut diff_min_y = u32::MAX;
  let mut diff_max_x = 0u32;
  let mut diff_max_y = 0u32;

  for y in 0..height {
    for x in 0..width {
      let base = (y * width + x) * 4;
      let sa = &serial_data[base..base + 4];
      let pa = &parallel_data[base..base + 4];
      if sa == pa {
        continue;
      }
      if first_mismatch.is_none() {
        first_mismatch = Some((x, y, sa.try_into().unwrap(), pa.try_into().unwrap()));
      }
      diff_min_x = diff_min_x.min(x as u32);
      diff_min_y = diff_min_y.min(y as u32);
      diff_max_x = diff_max_x.max(x as u32);
      diff_max_y = diff_max_y.max(y as u32);
    }
  }

  if let Some((x, y, sa, pa)) = first_mismatch {
    panic!(
      "pixmaps differ at ({x},{y}): serial={sa:?} parallel={pa:?}; diff_bbox=({diff_min_x},{diff_min_y})-({diff_max_x},{diff_max_y})"
    );
  }

  panic!("pixmaps differ, but could not locate mismatch");
}

fn build_svg_filter() -> Arc<fastrender::paint::svg_filter::SvgFilter> {
  // Includes a Gaussian blur (shared blur cache path) and a non-blur primitive
  // (feColorMatrix) so the full SVG filter pipeline runs.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0">
      <filter id="f" color-interpolation-filters="sRGB">
        <feGaussianBlur in="SourceGraphic" stdDeviation="2" result="blur" />
        <feColorMatrix
          in="blur"
          type="matrix"
          values="
            0 0 1 0 0
            0 1 0 0 0
            1 0 0 0 0
            0 0 0 1 0
          "
        />
      </filter>
    </svg>
  "#;
  let image_cache = ImageCache::new();
  parse_svg_filter_from_svg_document(svg, Some("f"), &image_cache).expect("parse svg filter")
}

fn build_display_list(filter: Arc<fastrender::paint::svg_filter::SvgFilter>) -> DisplayList {
  let mut list = DisplayList::new();
  // Create many identical filtered stacking contexts across different tiles. Each context produces
  // the same intermediate pixmap bytes, so parallel tile scheduling changes cache warm-up order for
  // both the shared blur cache (within a render call) and the global SVG filter result cache (across
  // renders).
  //
  // The contexts are kept well inside each 32×32 tile so the expected output does not depend on
  // cross-tile blur edge handling; the goal is to catch nondeterminism from parallelism/caches.
  let tile = 32.0;
  let inset = 6.0;
  let ctx_size = tile - inset * 2.0;
  let cell = 6.0;
  let pad = 4.0;
  for ty in 0..(HEIGHT / 32) {
    for tx in 0..(WIDTH / 32) {
      let origin_x = tx as f32 * tile + inset;
      let origin_y = ty as f32 * tile + inset;
      let bounds = Rect::from_xywh(origin_x, origin_y, ctx_size, ctx_size);
      list.push(DisplayItem::PushStackingContext(StackingContextItem {
        z_index: 0,
        creates_stacking_context: true,
        is_root: false,
        establishes_backdrop_root: true,
        has_backdrop_sensitive_descendants: false,
        bounds,
        plane_rect: bounds,
        mix_blend_mode: BlendMode::Normal,
        opacity: 1.0,
        is_isolated: true,
        transform: None,
        child_perspective: None,
        transform_style: TransformStyle::Flat,
        backface_visibility: BackfaceVisibility::Visible,
        filters: vec![ResolvedFilter::SvgFilter(filter.clone())],
        backdrop_filters: Vec::new(),
        radii: Default::default(),
        mask: None,
        has_clip_path: false,
      }));

      // A simple 2×2 checker pattern inside the stacking context.
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(origin_x + pad, origin_y + pad, cell, cell),
        color: Rgba::new(255, 0, 0, 1.0),
      }));
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(origin_x + pad + cell, origin_y + pad, cell, cell),
        color: Rgba::new(0, 0, 255, 1.0),
      }));
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(origin_x + pad, origin_y + pad + cell, cell, cell),
        color: Rgba::new(0, 0, 255, 1.0),
      }));
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(origin_x + pad + cell, origin_y + pad + cell, cell, cell),
        color: Rgba::new(255, 0, 0, 1.0),
      }));

      list.push(DisplayItem::PopStackingContext);
    }
  }
  list
}

fn build_seam_svg_filter() -> Arc<fastrender::paint::svg_filter::SvgFilter> {
  // The filter region is intentionally set to match the bbox exactly so the only required halo
  // comes from kernel sampling (feGaussianBlur), not from an expanded filter region.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0">
      <filter id="f" filterUnits="objectBoundingBox" x="0" y="0" width="1" height="1" color-interpolation-filters="sRGB">
        <feGaussianBlur in="SourceGraphic" stdDeviation="3" result="blur" />
        <feColorMatrix
          in="blur"
          type="matrix"
          values="
            1 0 0 0 0
            0 1 0 0 0
            0 0 1 0 0
            0 0 0 1 0
          "
        />
      </filter>
    </svg>
  "#;
  let image_cache = ImageCache::new();
  parse_svg_filter_from_svg_document(svg, Some("f"), &image_cache).expect("parse svg filter")
}

fn build_seam_display_list(filter: Arc<fastrender::paint::svg_filter::SvgFilter>) -> DisplayList {
  let mut list = DisplayList::new();

  // A single stacking context that crosses the x=32 tile boundary (tile_size=32).
  // Without sufficient halo for feGaussianBlur, parallel tiled paint will treat the tile boundary
  // as transparent and produce seams vs serial rendering.
  let bounds = Rect::from_xywh(16.0, 4.0, 32.0, 24.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::SvgFilter(filter)],
    backdrop_filters: Vec::new(),
    radii: Default::default(),
    mask: None,
    has_clip_path: false,
  }));

  // Solid content spanning the tile boundary.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::new(0, 0, 0, 1.0),
  }));

  list.push(DisplayItem::PopStackingContext);
  list
}

#[test]
fn svg_filter_parallel_paint_is_byte_identical_and_deterministic() {
  let filter = build_svg_filter();
  let list = build_display_list(filter);
  let font_ctx = FontContext::new();
  let cpu_budget = fastrender::system::cpu_budget();

  let serial_pixmap = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");
  let baseline = serial_pixmap.data().to_vec();

  let parallelism = PaintParallelism {
    tile_size: 32,
    log_timing: false,
    min_display_items: 1,
    min_tiles: 1,
    min_build_fragments: 1,
    build_chunk_size: 1,
    max_threads: Some(4),
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");

  // (A) Serial vs parallel match.
  let first = pool.install(|| {
    DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });
  if cpu_budget > 1 {
    assert!(
      first.parallel_used,
      "expected svg-filter scene to use parallel tiling (fallback={:?})",
      first.fallback_reason
    );
    assert!(first.tiles > 1, "expected multiple tiles to be rendered");
  }
  assert_pixmap_eq(&serial_pixmap, &first.pixmap);

  // (B) Parallel determinism under repeated/concurrent runs. This stresses shared caches (blur
  // cache + SVG filter result cache) under different work-stealing interleavings.
  const ITERATIONS: usize = 32;
  let outputs: Vec<Vec<u8>> = pool.install(|| {
    (0..ITERATIONS)
      .into_par_iter()
      .map(|_| {
        let report = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
          .expect("renderer")
          .with_parallelism(parallelism)
          .render_with_report(&list)
          .expect("parallel render");
        if cpu_budget > 1 {
          assert!(
            report.parallel_used,
            "expected parallel tiling on repeated svg-filter render (fallback={:?})",
            report.fallback_reason
          );
        }
        report.pixmap.data().to_vec()
      })
      .collect()
  });

  for (idx, output) in outputs.iter().enumerate() {
    assert_eq!(
      output.as_slice(),
      baseline.as_slice(),
      "parallel svg-filter output diverged from serial baseline (iteration {idx})"
    );
  }
}

#[test]
fn svg_filter_parallel_matches_serial_across_tile_boundaries() {
  let filter = build_seam_svg_filter();
  let list = build_seam_display_list(filter);
  let font_ctx = FontContext::new();

  const SEAM_WIDTH: u32 = 64;
  const SEAM_HEIGHT: u32 = 32;

  let serial_pixmap =
    DisplayListRenderer::new(SEAM_WIDTH, SEAM_HEIGHT, Rgba::WHITE, font_ctx.clone())
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render(&list)
      .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 32,
    log_timing: false,
    min_display_items: 1,
    min_tiles: 1,
    min_build_fragments: 1,
    build_chunk_size: 1,
    max_threads: Some(4),
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");

  let parallel = pool.install(|| {
    DisplayListRenderer::new(SEAM_WIDTH, SEAM_HEIGHT, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  assert!(
    parallel.parallel_used,
    "expected svg-filter scene to use parallel tiling (fallback={:?})",
    parallel.fallback_reason
  );
  assert!(parallel.tiles > 1, "expected multiple tiles to be rendered");
  assert_pixmap_eq(&serial_pixmap, &parallel.pixmap);
}
