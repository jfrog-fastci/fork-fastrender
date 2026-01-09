use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ImageData, MaskReferenceRects,
  ResolvedMask, ResolvedMaskImage, ResolvedMaskLayer, StackingContextItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{
  BackfaceVisibility, BackgroundRepeat, BackgroundSize, BackgroundSizeComponent, MaskClip,
  MaskComposite, MaskLayer, MaskMode, MaskOrigin, TransformStyle,
};
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use std::collections::HashMap;
use std::sync::Arc;

fn context(bounds: Rect, transform_style: TransformStyle) -> StackingContextItem {
  StackingContextItem {
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
    transform_style,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }
}

fn mask_rects(bounds: Rect) -> MaskReferenceRects {
  MaskReferenceRects {
    border: bounds,
    padding: bounds,
    content: bounds,
  }
}

#[test]
fn preserve_3d_root_mask_applies_to_composed_scene_output() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let rect = bounds;

  // Mask image: left half transparent, right half opaque.
  let mut pixels = vec![0u8; (10 * 10 * 4) as usize];
  for y in 0..10u32 {
    for x in 0..10u32 {
      let idx = ((y * 10 + x) * 4) as usize;
      pixels[idx] = 0;
      pixels[idx + 1] = 0;
      pixels[idx + 2] = 0;
      pixels[idx + 3] = if x < 5 { 0 } else { 255 };
    }
  }
  let image = ImageData::new_pixels(10, 10, pixels);

  let mut layer_style = MaskLayer::default();
  layer_style.repeat = BackgroundRepeat::no_repeat();
  layer_style.size = BackgroundSize::Explicit(
    BackgroundSizeComponent::Length(Length::percent(100.0)),
    BackgroundSizeComponent::Length(Length::percent(100.0)),
  );
  layer_style.mode = MaskMode::Alpha;
  layer_style.origin = MaskOrigin::BorderBox;
  layer_style.clip = MaskClip::BorderBox;
  layer_style.composite = MaskComposite::Add;

  let mask = ResolvedMask {
    layers: vec![ResolvedMaskLayer {
      image: ResolvedMaskImage::Raster(image),
      repeat: layer_style.repeat,
      position: layer_style.position,
      size: layer_style.size,
      origin: layer_style.origin,
      clip: layer_style.clip,
      mode: layer_style.mode,
      composite: layer_style.composite,
    }],
    color: Rgba::BLACK,
    used_dark_color_scheme: false,
    forced_colors: false,
    font_size: 16.0,
    root_font_size: 16.0,
    viewport: None,
    rects: mask_rects(bounds),
  };

  let mut list = DisplayList::new();
  let mut root = context(bounds, TransformStyle::Preserve3d);
  root.mask = Some(mask);
  list.push(DisplayItem::PushStackingContext(root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);

  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PRESERVE3D_DISABLE_SCENE".to_string(),
    "0".to_string(),
  )])));
  let pixmap = with_thread_runtime_toggles(toggles, || {
    DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap()
  });

  let left = pixmap.pixel(2, 5).expect("pixel in-bounds");
  assert_eq!(
    (left.red(), left.green(), left.blue(), left.alpha()),
    (255, 255, 255, 255),
    "expected mask alpha=0 to clip the composed preserve-3d scene output"
  );

  let right = pixmap.pixel(7, 5).expect("pixel in-bounds");
  assert_eq!(
    (right.red(), right.green(), right.blue(), right.alpha()),
    (255, 0, 0, 255),
    "expected mask alpha=1 to preserve the composed preserve-3d scene output"
  );
}
