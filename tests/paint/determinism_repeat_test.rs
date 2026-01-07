use fastrender::paint::clip_path::ResolvedClipPath;
use fastrender::paint::display_list::{
  BlendMode, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem, ImageData,
  MaskReferenceRects, ResolvedFilter, ResolvedMask, ResolvedMaskImage, ResolvedMaskLayer,
  StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::paint::scratch::reset_thread_local_scratch;
use fastrender::style::types::{
  BackfaceVisibility, BackgroundPosition, BackgroundPositionComponent, BackgroundRepeat,
  BackgroundSize, BackgroundSizeComponent, MaskClip, MaskComposite, MaskMode, MaskOrigin,
  TransformStyle,
};
use fastrender::text::font_loader::FontContext;
use fastrender::{BorderRadii, Length, Point, Rect, Rgba};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use std::sync::mpsc;
use std::sync::Arc;

fn assert_rgba8888_pixels_eq(width: u32, height: u32, expected: &[u8], actual: &[u8], label: &str) {
  assert_eq!(
    expected.len(),
    actual.len(),
    "{label}: pixel buffer sizes differ"
  );
  assert_eq!(
    expected.len(),
    width as usize * height as usize * 4,
    "{label}: expected buffer is not width*height*4"
  );

  if expected == actual {
    return;
  }

  let mut mismatched_bytes = 0usize;
  let mut mismatched_pixels = 0usize;
  let mut first: Option<(usize, [u8; 4], [u8; 4])> = None;
  let mut min_x = usize::MAX;
  let mut min_y = usize::MAX;
  let mut max_x = 0usize;
  let mut max_y = 0usize;
  let mut samples: Vec<(usize, usize, [u8; 4], [u8; 4])> = Vec::new();
  for (idx, (a, b)) in expected
    .chunks_exact(4)
    .zip(actual.chunks_exact(4))
    .enumerate()
  {
    let a = [a[0], a[1], a[2], a[3]];
    let b = [b[0], b[1], b[2], b[3]];
    if a != b {
      mismatched_pixels += 1;
      mismatched_bytes += a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
      if first.is_none() {
        first = Some((idx, a, b));
      }
      let x = idx % (width as usize);
      let y = idx / (width as usize);
      min_x = min_x.min(x);
      min_y = min_y.min(y);
      max_x = max_x.max(x);
      max_y = max_y.max(y);
      if samples.len() < 16 {
        samples.push((x, y, a, b));
      }
    }
  }

  if let Some((idx, a, b)) = first {
    let x = idx % (width as usize);
    let y = idx / (width as usize);
    panic!(
      "{label}: {mismatched_pixels} pixels ({mismatched_bytes} bytes) differ; bounds=({min_x},{min_y})..=({max_x},{max_y}); first at ({x}, {y}) expected={a:?} actual={b:?}; sample={samples:?}"
    );
  }
  panic!("{label}: buffers differ");
}

fn top_left_position() -> BackgroundPosition {
  BackgroundPosition::Position {
    x: BackgroundPositionComponent {
      alignment: 0.0,
      offset: Length::percent(0.0),
    },
    y: BackgroundPositionComponent {
      alignment: 0.0,
      offset: Length::percent(0.0),
    },
  }
}

fn patterned_mask(bounds: Rect) -> ResolvedMask {
  const SIZE: u32 = 8;
  let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
  for y in 0..SIZE {
    for x in 0..SIZE {
      let base = x * 32 + y * 4;
      let alpha = if base < 24 {
        0
      } else if base > 224 {
        255
      } else {
        base as u8
      };
      pixels.extend_from_slice(&[0, 0, 0, alpha]);
    }
  }

  ResolvedMask {
    layers: vec![ResolvedMaskLayer {
      image: ResolvedMaskImage::Raster(ImageData::new_pixels(SIZE, SIZE, pixels)),
      repeat: BackgroundRepeat::repeat(),
      position: top_left_position(),
      size: BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto),
      origin: MaskOrigin::BorderBox,
      clip: MaskClip::BorderBox,
      mode: MaskMode::Alpha,
      composite: MaskComposite::Add,
    }],
    color: Rgba::BLACK,
    font_size: 16.0,
    root_font_size: 16.0,
    viewport: None,
    rects: MaskReferenceRects {
      border: bounds,
      padding: bounds,
      content: bounds,
    },
  }
}

fn filter_backdrop_mask_clip_scene(width: u32, height: u32) -> DisplayList {
  let mut list = DisplayList::new();

  // Split-color backdrop so blur/saturate are visually meaningful.
  let half = width as f32 / 2.0;
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, half, height as f32),
    color: Rgba::rgb(255, 0, 0),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(half, 0.0, half, height as f32),
    color: Rgba::rgb(0, 0, 255),
  }));

  // Clip-path style polygon to exercise path rasterization + clip mask compositing.
  let clip_path = ResolvedClipPath::Polygon {
    points: vec![
      Point::new(22.0, 28.0),
      Point::new(width as f32 - 18.0, 18.0),
      Point::new(width as f32 - 24.0, height as f32 - 22.0),
      Point::new(28.0, height as f32 - 14.0),
    ],
    fill_rule: tiny_skia::FillRule::Winding,
  };
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path { path: clip_path },
  }));

  let backdrop_bounds = Rect::from_xywh(24.0, 16.0, width as f32 - 48.0, height as f32 - 32.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: true,
    bounds: backdrop_bounds,
    plane_rect: backdrop_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: vec![ResolvedFilter::Blur(4.0), ResolvedFilter::Saturate(1.6)],
    radii: BorderRadii::uniform(12.0),
    mask: None,
    has_clip_path: false,
  }));

  // Masked layer under the backdrop-filter effect.
  let masked_bounds = Rect::from_xywh(44.0, 34.0, width as f32 - 88.0, height as f32 - 68.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds: masked_bounds,
    plane_rect: masked_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::uniform(10.0),
    mask: Some(patterned_mask(masked_bounds)),
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: masked_bounds,
    color: Rgba::new(255, 255, 255, 0.35),
  }));

  // Drop-shadow filter to exercise filter outsets and intermediate pixmap copies.
  let shadow_bounds = Rect::from_xywh(64.0, 58.0, width as f32 - 128.0, height as f32 - 124.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 1,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds: shadow_bounds,
    plane_rect: shadow_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::DropShadow {
      offset_x: 6.0,
      offset_y: 4.0,
      blur_radius: 6.0,
      spread: 2.0,
      color: Rgba::new(0, 0, 0, 0.55),
    }],
    backdrop_filters: Vec::new(),
    radii: BorderRadii::uniform(8.0),
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: shadow_bounds,
    color: Rgba::new(40, 160, 220, 0.9),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(
      shadow_bounds.x() + 10.0,
      shadow_bounds.y() + 18.0,
      shadow_bounds.width() - 20.0,
      14.0,
    ),
    color: Rgba::new(255, 210, 50, 0.7),
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopClip);

  list
}

fn preserve_3d_backdrop_scene(width: u32, height: u32) -> DisplayList {
  let mut list = DisplayList::new();

  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, width as f32, height as f32),
    color: Rgba::rgb(250, 250, 250),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, width as f32, height as f32 / 2.0),
    color: Rgba::rgb(220, 220, 220),
  }));

  let root_bounds = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: root_bounds,
    plane_rect: root_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: Some(Transform3D::perspective(420.0)),
    transform_style: TransformStyle::Preserve3d,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));

  let far_plane = Rect::from_xywh(18.0, 24.0, 132.0, 132.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: far_plane,
    plane_rect: far_plane,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(Transform3D::translate(0.0, 0.0, -40.0)),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: far_plane,
    color: Rgba::new(200, 60, 120, 0.85),
  }));
  list.push(DisplayItem::PopStackingContext);

  let mid_plane = Rect::from_xywh(40.0, 40.0, 124.0, 124.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: true,
    bounds: mid_plane,
    plane_rect: mid_plane,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(Transform3D::translate(0.0, 0.0, 0.0)),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    // Backdrop-filter within a preserve-3d plane is a known risk area for nondeterministic pixels.
    backdrop_filters: vec![ResolvedFilter::Blur(3.0), ResolvedFilter::Saturate(1.4)],
    radii: BorderRadii::uniform(12.0),
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: mid_plane,
    color: Rgba::new(255, 255, 255, 0.25),
  }));
  list.push(DisplayItem::PopStackingContext);

  let near_plane = Rect::from_xywh(62.0, 58.0, 104.0, 104.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: near_plane,
    plane_rect: near_plane,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(Transform3D::translate(0.0, 0.0, 36.0)),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: near_plane,
    color: Rgba::new(30, 140, 230, 0.75),
  }));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext);
  list
}

fn render_bytes(
  width: u32,
  height: u32,
  list: &DisplayList,
  font_ctx: FontContext,
  parallelism: PaintParallelism,
  reset_scratch: bool,
) -> Vec<u8> {
  if reset_scratch {
    reset_thread_local_scratch();
  }
  let pixmap = DisplayListRenderer::new(width, height, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(parallelism)
    .render(list)
    .expect("render");
  pixmap.data().to_vec()
}

fn run_on_pool<T: Send + 'static>(
  pool: &rayon::ThreadPool,
  job: impl FnOnce() -> T + Send + 'static,
) -> T {
  let (tx, rx) = mpsc::channel();
  pool.spawn(move || {
    tx.send(job()).expect("send result");
  });
  rx.recv().expect("receive result")
}

#[test]
fn paint_is_repeatable_under_parallel_scheduling() {
  const WIDTH: u32 = 192;
  const HEIGHT: u32 = 192;
  const REPEATS_PER_SCENE: usize = 20;
  const SERIAL_REPEATS_PER_SCENE: usize = 5;

  let parallelism = PaintParallelism {
    tile_size: 32,
    log_timing: false,
    min_display_items: 1,
    min_tiles: 1,
    min_build_fragments: 1,
    build_chunk_size: 1,
    ..PaintParallelism::enabled()
  };

  let scene_a = Arc::new(filter_backdrop_mask_clip_scene(WIDTH, HEIGHT));
  let scene_b = Arc::new(preserve_3d_backdrop_scene(WIDTH, HEIGHT));
  let font_ctx = FontContext::new();

  // Baselines are rendered in fresh single-thread pools so the expected output is independent of
  // any prior work that might have populated thread-local scratch buffers in other tests.
  let baseline_a = {
    let pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let list = scene_a.clone();
    let font_ctx = font_ctx.clone();
    run_on_pool(&pool, move || {
      render_bytes(WIDTH, HEIGHT, &list, font_ctx, parallelism, false)
    })
  };
  let baseline_b = {
    let pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let list = scene_b.clone();
    let font_ctx = font_ctx.clone();
    run_on_pool(&pool, move || {
      render_bytes(WIDTH, HEIGHT, &list, font_ctx, parallelism, false)
    })
  };

  // Run the repeat suite twice:
  // - Once without any scratch resets to ensure TLS reuse does not leak state.
  // - Once with explicit scratch resets so tests can force a clean slate between renders.
  for reset_scratch in [false, true] {
    let mode = if reset_scratch { "reset" } else { "reuse" };

    // Ensure serial rerenders do not depend on earlier work on the same thread (e.g. scratch buffers
    // not fully initialized between calls).
    let serial_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    for i in 0..SERIAL_REPEATS_PER_SCENE {
      let list = scene_a.clone();
      let font_ctx = font_ctx.clone();
      let bytes = run_on_pool(&serial_pool, move || {
        render_bytes(WIDTH, HEIGHT, &list, font_ctx, parallelism, reset_scratch)
      });
      assert_rgba8888_pixels_eq(
        WIDTH,
        HEIGHT,
        &baseline_a,
        &bytes,
        &format!("scene A {mode} serial repeat {i}"),
      );
    }
    for i in 0..SERIAL_REPEATS_PER_SCENE {
      let list = scene_b.clone();
      let font_ctx = font_ctx.clone();
      let bytes = run_on_pool(&serial_pool, move || {
        render_bytes(WIDTH, HEIGHT, &list, font_ctx, parallelism, reset_scratch)
      });
      assert_rgba8888_pixels_eq(
        WIDTH,
        HEIGHT,
        &baseline_b,
        &bytes,
        &format!("scene B {mode} serial repeat {i}"),
      );
    }

    // Interleave both scenes on the same thread to catch state leakage across unrelated workloads.
    for i in 0..(SERIAL_REPEATS_PER_SCENE * 2) {
      let (expected, list, label) = if i % 2 == 0 {
        (
          &baseline_a,
          scene_a.clone(),
          format!("{mode} serial interleave {i} scene A"),
        )
      } else {
        (
          &baseline_b,
          scene_b.clone(),
          format!("{mode} serial interleave {i} scene B"),
        )
      };
      let font_ctx = font_ctx.clone();
      let bytes = run_on_pool(&serial_pool, move || {
        render_bytes(WIDTH, HEIGHT, &list, font_ctx, parallelism, reset_scratch)
      });
      assert_rgba8888_pixels_eq(WIDTH, HEIGHT, expected, &bytes, &label);
    }

    // Interleave renders of both scenes in parallel to maximize reuse of thread-local scratch between
    // different workloads. Any nondeterministic pixels (from partially initialized buffers, etc)
    // should show up as byte-level differences relative to the single-thread baseline.
    let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let results: Vec<(u8, Vec<u8>)> = pool.install(|| {
      (0..(REPEATS_PER_SCENE * 2))
        .into_par_iter()
        .map(|i| {
          if i % 2 == 0 {
            let bytes = render_bytes(
              WIDTH,
              HEIGHT,
              &scene_a,
              font_ctx.clone(),
              parallelism,
              reset_scratch,
            );
            (0u8, bytes)
          } else {
            let bytes = render_bytes(
              WIDTH,
              HEIGHT,
              &scene_b,
              font_ctx.clone(),
              parallelism,
              reset_scratch,
            );
            (1u8, bytes)
          }
        })
        .collect()
    });

    for (idx, (scene, bytes)) in results.iter().enumerate() {
      match scene {
        0 => assert_rgba8888_pixels_eq(
          WIDTH,
          HEIGHT,
          &baseline_a,
          bytes,
          &format!("parallel {mode} repeat {idx} scene A"),
        ),
        1 => assert_rgba8888_pixels_eq(
          WIDTH,
          HEIGHT,
          &baseline_b,
          bytes,
          &format!("parallel {mode} repeat {idx} scene B"),
        ),
        other => panic!("unexpected scene tag {other}"),
      }
    }
  }
}
