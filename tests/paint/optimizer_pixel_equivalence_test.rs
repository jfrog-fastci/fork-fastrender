//! Pixel-equivalence regression tests for `DisplayListOptimizer`.
//!
//! The optimizer performs structural rewrites (scope removal, culling, merging). These tests
//! ensure `optimize_checked` is semantics-preserving by rendering a small scene before and after
//! optimization and asserting byte-for-byte equality of the resulting pixmaps.

use fastrender::geometry::{Point, Rect};
use fastrender::paint::clip_path::ResolvedClipPath;
use fastrender::paint::display_list::{
  BlendMode, BlendModeItem, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem, ImageData,
  MaskReferenceRects, OpacityItem, ResolvedFilter, ResolvedMask, ResolvedMaskImage,
  ResolvedMaskLayer, StackingContextItem, Transform3D, TransformItem,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::style::types::{
  BackfaceVisibility, BackgroundPosition, BackgroundPositionComponent, BackgroundRepeat,
  BackgroundSize, BackgroundSizeComponent, MaskClip, MaskComposite, MaskMode, MaskOrigin,
  TransformStyle,
};
use fastrender::DisplayListOptimizer;
use fastrender::{BorderRadii, FontConfig, Length, Rgba};
use std::sync::OnceLock;
use tiny_skia::Pixmap;

fn shared_font_ctx() -> fastrender::text::font_loader::FontContext {
  static FONT_CTX: OnceLock<fastrender::text::font_loader::FontContext> = OnceLock::new();
  FONT_CTX
    .get_or_init(|| {
      fastrender::text::font_loader::FontContext::with_config(FontConfig::bundled_only())
    })
    .clone()
}

fn render_list(list: &DisplayList, width: u32, height: u32) -> Pixmap {
  DisplayListRenderer::new(width, height, Rgba::WHITE, shared_font_ctx())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(list)
    .expect("render")
}

fn assert_pixmap_eq(expected: &Pixmap, actual: &Pixmap, label: &str) {
  assert_eq!(
    expected.width(),
    actual.width(),
    "{label}: pixmap width mismatch"
  );
  assert_eq!(
    expected.height(),
    actual.height(),
    "{label}: pixmap height mismatch"
  );

  let expected_data = expected.data();
  let actual_data = actual.data();
  if expected_data == actual_data {
    return;
  }

  let width = expected.width() as usize;
  let mut mismatched_pixels = 0usize;
  let mut first: Option<(usize, [u8; 4], [u8; 4])> = None;
  for (idx, (a, b)) in expected_data
    .chunks_exact(4)
    .zip(actual_data.chunks_exact(4))
    .enumerate()
  {
    let a = [a[0], a[1], a[2], a[3]];
    let b = [b[0], b[1], b[2], b[3]];
    if a != b {
      mismatched_pixels += 1;
      if first.is_none() {
        first = Some((idx, a, b));
      }
    }
  }

  if let Some((idx, a, b)) = first {
    let x = idx % width;
    let y = idx / width;
    panic!(
      "{label}: {mismatched_pixels} pixels differ; first mismatch at ({x}, {y}) expected={a:?} actual={b:?}"
    );
  }
  panic!("{label}: pixmaps differ");
}

fn assert_optimizer_preserves_pixels(list: DisplayList, width: u32, height: u32, label: &str) {
  let before = render_list(&list, width, height);
  let viewport = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let (optimized, _stats) = DisplayListOptimizer::new()
    .optimize_checked(&list, viewport)
    .expect("optimize_checked");
  let after = render_list(&optimized, width, height);
  assert_pixmap_eq(&before, &after, label);
}

fn fill(rect: Rect, color: Rgba) -> DisplayItem {
  DisplayItem::FillRect(FillRectItem { rect, color })
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

fn simple_alpha_mask(bounds: Rect) -> ResolvedMask {
  const SIZE: u32 = 8;
  let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
  for y in 0..SIZE {
    for x in 0..SIZE {
      // Diagonal gradient alpha for easy visual differences.
      let alpha = (((x + y) * 255) / (2 * (SIZE - 1))).clamp(0, 255) as u8;
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

#[test]
fn optimizer_pixel_equivalence_backface_culling() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));

  // Flip the X axis around the right edge: x' = -x + width.
  let flip =
    Transform3D::translate(width as f32, 0.0, 0.0).multiply(&Transform3D::scale(-1.0, 1.0, 1.0));
  list.push(DisplayItem::PushTransform(TransformItem {
    transform: flip,
  }));

  // Backface-visibility should be respected even when the stacking context is otherwise a no-op.
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: canvas,
    plane_rect: canvas,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Hidden,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(fill(Rect::from_xywh(10.0, 10.0, 20.0, 20.0), Rgba::RED));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopTransform);

  assert_optimizer_preserves_pixels(list, width, height, "backface_culling");
}

#[test]
fn optimizer_pixel_equivalence_opacity_scopes() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));
  list.push(fill(Rect::from_xywh(8.0, 8.0, 48.0, 48.0), Rgba::BLUE));

  list.push(DisplayItem::PushOpacity(OpacityItem { opacity: 0.5 }));
  list.push(fill(Rect::from_xywh(12.0, 12.0, 40.0, 40.0), Rgba::RED));
  list.push(DisplayItem::PushOpacity(OpacityItem { opacity: 0.5 }));
  list.push(fill(Rect::from_xywh(16.0, 16.0, 32.0, 32.0), Rgba::GREEN));
  list.push(DisplayItem::PopOpacity);
  list.push(DisplayItem::PopOpacity);

  assert_optimizer_preserves_pixels(list, width, height, "opacity_scopes");
}

#[test]
fn optimizer_pixel_equivalence_blend_mode_group() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));
  list.push(fill(
    Rect::from_xywh(0.0, 0.0, 64.0, 64.0),
    Rgba::rgb(240, 240, 255),
  ));
  list.push(fill(
    Rect::from_xywh(8.0, 8.0, 48.0, 48.0),
    Rgba::rgb(200, 60, 60),
  ));

  list.push(DisplayItem::PushBlendMode(BlendModeItem {
    mode: BlendMode::Multiply,
  }));
  // Semi-transparent layer so blend math is observable.
  list.push(fill(
    Rect::from_xywh(16.0, 16.0, 32.0, 32.0),
    Rgba::new(60, 200, 60, 0.75),
  ));
  list.push(DisplayItem::PopBlendMode);

  assert_optimizer_preserves_pixels(list, width, height, "blend_mode_group");
}

#[test]
fn optimizer_pixel_equivalence_filter_outset() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));

  // Content is fully offscreen, but blur extends into the viewport.
  let bounds = Rect::from_xywh(-8.0, 20.0, 4.0, 24.0);
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
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::Blur(4.0)],
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(fill(bounds, Rgba::BLACK));
  list.push(DisplayItem::PopStackingContext);

  assert_optimizer_preserves_pixels(list, width, height, "filter_outset");
}

#[test]
fn optimizer_pixel_equivalence_mask() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let bounds = Rect::from_xywh(8.0, 8.0, 48.0, 48.0);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));
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
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: Some(simple_alpha_mask(bounds)),
    has_clip_path: false,
  }));
  list.push(fill(bounds, Rgba::rgb(40, 120, 220)));
  list.push(DisplayItem::PopStackingContext);

  assert_optimizer_preserves_pixels(list, width, height, "mask");
}

#[test]
fn optimizer_pixel_equivalence_clip_rect_and_path() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));

  let clip_rect = Rect::from_xywh(8.0, 8.0, 48.0, 48.0);
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Rect {
      rect: clip_rect,
      radii: None,
    },
  }));
  list.push(fill(canvas, Rgba::rgb(240, 200, 200)));

  let clip_path = ResolvedClipPath::Polygon {
    points: vec![
      Point::new(16.0, 16.0),
      Point::new(56.0, 20.0),
      Point::new(40.0, 56.0),
      Point::new(12.0, 44.0),
    ],
    fill_rule: tiny_skia::FillRule::Winding,
  };
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path { path: clip_path },
  }));
  list.push(fill(canvas, Rgba::rgb(120, 180, 240)));
  list.push(DisplayItem::PopClip);
  list.push(DisplayItem::PopClip);

  assert_optimizer_preserves_pixels(list, width, height, "clip_rect_and_path");
}

#[test]
fn optimizer_pixel_equivalence_identity_transform_scope() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));
  list.push(DisplayItem::PushTransform(TransformItem {
    transform: Transform3D::identity(),
  }));
  list.push(fill(Rect::from_xywh(16.0, 16.0, 32.0, 32.0), Rgba::GREEN));
  list.push(DisplayItem::PopTransform);

  assert_optimizer_preserves_pixels(list, width, height, "identity_transform");
}

#[test]
fn optimizer_pixel_equivalence_noop_stacking_context() {
  let (width, height) = (64, 64);
  let canvas = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let bounds = Rect::from_xywh(0.0, 0.0, 64.0, 64.0);

  let mut list = DisplayList::new();
  list.push(fill(canvas, Rgba::WHITE));
  list.push(fill(
    Rect::from_xywh(0.0, 0.0, 32.0, 64.0),
    Rgba::rgb(220, 240, 220),
  ));

  // A stacking context with only z-index metadata should be a no-op.
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(fill(
    Rect::from_xywh(20.0, 20.0, 24.0, 24.0),
    Rgba::rgb(240, 80, 80),
  ));
  list.push(DisplayItem::PopStackingContext);

  list.push(fill(
    Rect::from_xywh(32.0, 0.0, 32.0, 64.0),
    Rgba::rgb(220, 220, 240),
  ));

  assert_optimizer_preserves_pixels(list, width, height, "noop_stacking_context");
}
