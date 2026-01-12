use crate::paint::display_list::{
  BlendMode, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
  Transform3D,
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

  // Regression test:
  //
  // Tile halo estimation must account for stacking context transforms. A CSS filter is defined in
  // the element's local coordinate system; when a scale transform is applied, the filter kernel is
  // effectively scaled in the output. If the tile halo ignores that transform scale, parallel
  // tiling can clip filter input pixels near tile edges, producing seams vs serial rendering.
  //
  // Place the transformed stacking context so it crosses an interior tile boundary (x=64 when
  // tile_size=32) so neither tile's halo is clamped by the canvas edge.
  let bounds = Rect::from_xywh(16.0, 8.0, 32.0, 16.0);
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
    transform: Some(Transform3D::scale(2.0, 2.0, 1.0)),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::Blur(3.0)],
    backdrop_filters: Vec::new(),
    radii: Default::default(),
    mask: None,
    mask_border: None,
    has_clip_path: false,
  }));

  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::new(255, 0, 0, 1.0),
  }));

  list.push(DisplayItem::PopStackingContext);
  list
}

#[test]
fn filter_blur_scale_transform_parallel_matches_serial_across_tile_boundaries() {
  const WIDTH: u32 = 128;
  const HEIGHT: u32 = 64;

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
      "expected filtered scene to use parallel tiling (fallback={:?})",
      parallel.fallback_reason
    );
    assert!(parallel.tiles > 1, "expected multiple tiles to be rendered");
  }
  assert_pixmap_eq(&serial_pixmap, &parallel.pixmap);
}
