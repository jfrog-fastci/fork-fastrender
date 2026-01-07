use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use rayon::ThreadPoolBuilder;

fn preserve_3d_list(bounds: Rect) -> DisplayList {
  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Preserve3d,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));

  for (z, color) in [(0, Rgba::RED), (1, Rgba::GREEN)] {
    list.push(DisplayItem::PushStackingContext(StackingContextItem {
      z_index: z,
      creates_stacking_context: true,
      is_root: false,
      establishes_backdrop_root: false,
      bounds,
      plane_rect: bounds,
      mix_blend_mode: BlendMode::Normal,
      opacity: 1.0,
      is_isolated: false,
      transform: Some(Transform3D::translate(0.0, 0.0, z as f32)),
      child_perspective: None,
      transform_style: TransformStyle::Preserve3d,
      backface_visibility: BackfaceVisibility::Visible,
      filters: Vec::new(),
      backdrop_filters: Vec::new(),
      radii: BorderRadii::ZERO,
      mask: None,
      has_clip_path: false,
    }));

    for _ in 0..160 {
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: bounds,
        color,
      }));
    }

    list.push(DisplayItem::PopStackingContext);
  }

  list.push(DisplayItem::PopStackingContext);
  list
}

#[test]
fn preserve_3d_renders_are_deterministic_with_parallel_paint_enabled() {
  let bounds = Rect::from_xywh(0.0, 0.0, 260.0, 260.0);
  let list = preserve_3d_list(bounds);

  let cpu_budget = fastrender::system::cpu_budget();
  let parallelism = PaintParallelism {
    tile_size: 64,
    min_display_items: 1,
    min_tiles: 1,
    min_build_fragments: 1,
    build_chunk_size: 1,
    ..PaintParallelism::enabled()
  };

  let font_ctx = FontContext::new();
  let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();
  pool.install(|| {
    let mut baseline: Option<Vec<u8>> = None;
    for iteration in 0..15 {
      let report = DisplayListRenderer::new(260, 260, Rgba::WHITE, font_ctx.clone())
        .unwrap()
        .with_parallelism(parallelism)
        .render_with_report(&list)
        .unwrap();

      assert!(
        !report.parallel_used,
        "preserve-3d stacking contexts should disable outer tiling (iteration {iteration})"
      );
      if cpu_budget > 1 {
        assert_eq!(
          report.fallback_reason.as_deref(),
          Some("preserve-3d stacking contexts require serial painting"),
          "unexpected parallel fallback reason (iteration {iteration})"
        );
      }

      let bytes = report.pixmap.data().to_vec();
      match &baseline {
        Some(expected) => assert_eq!(expected, &bytes, "render output differed at {iteration}"),
        None => baseline = Some(bytes),
      }
    }
  });
}
