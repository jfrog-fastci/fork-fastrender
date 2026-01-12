use crate::paint::display_list::{
  BlendMode, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
};
use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use crate::style::types::{BackfaceVisibility, TransformStyle};
use crate::text::font_loader::FontContext;
use crate::{Rect, Rgba};
use rayon::ThreadPoolBuilder;
use tiny_skia::Pixmap;

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

fn build_display_list() -> DisplayList {
  let mut list = DisplayList::new();

  // A single stacking context that crosses the x=32 tile boundary (tile_size=32).
  //
  // The filter is `drop-shadow(..., spread:-N)` with zero blur and offset. Negative spread performs
  // an erosion pass; when the tile halo is computed using *visible* outsets, the renderer can omit
  // `abs(spread)` and each tile can observe the tile boundary as an artificial edge, producing a
  // seam vs serial rendering.
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
    filters: vec![ResolvedFilter::DropShadow {
      offset_x: 0.0,
      offset_y: 0.0,
      blur_radius: 0.0,
      spread: -8.0,
      color: Rgba::new(0, 0, 0, 1.0),
    }],
    backdrop_filters: Vec::new(),
    radii: Default::default(),
    mask: None,
    mask_border: None,
    has_clip_path: false,
  }));

  // Solid content spanning the tile boundary. Use semi-transparent fill so the shadow drawn behind
  // the content contributes to the final pixels (making seams observable).
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::new(255, 0, 0, 0.5),
  }));

  list.push(DisplayItem::PopStackingContext);
  list
}

#[test]
fn drop_shadow_negative_spread_parallel_matches_serial_across_tile_boundaries() {
  const WIDTH: u32 = 64;
  const HEIGHT: u32 = 32;

  let list = build_display_list();
  let font_ctx = FontContext::new();
  let cpu_budget = crate::system::cpu_budget();

  let serial_pixmap = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 32,
    log_timing: false,
    min_display_items: 1,
    min_tiles: 1,
    max_threads: Some(4),
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");

  let parallel = pool.install(|| {
    DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  if cpu_budget > 1 {
    assert!(
      parallel.parallel_used,
      "expected drop-shadow scene to use parallel tiling (fallback={:?})",
      parallel.fallback_reason
    );
    assert!(parallel.tiles > 1, "expected multiple tiles to be rendered");
  }
  assert_pixmap_eq(&serial_pixmap, &parallel.pixmap);
}
