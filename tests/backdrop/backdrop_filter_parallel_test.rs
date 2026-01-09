use base64::Engine;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::{
  BlendMode, BorderRadius, DisplayItem, DisplayList, FillRectItem, ImageData, MaskReferenceRects,
  ResolvedFilter, ResolvedMask, ResolvedMaskImage, ResolvedMaskLayer, StackingContextItem,
};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::style::types::{
  BackfaceVisibility, BackgroundPosition, BackgroundPositionComponent, BackgroundRepeat,
  BackgroundSize, BackgroundSizeComponent, MaskClip, MaskComposite, MaskMode, MaskOrigin,
  TransformStyle,
};
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{
  BorderRadii, DiagnosticsLevel, FastRender, FontConfig, Length, Point, Rect,
  RenderArtifactRequest, RenderOptions, Rgba,
};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use url::Url;

fn run_with_large_stack(f: impl FnOnce() + Send + 'static) {
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(f)
    .expect("spawn thread")
    .join()
    .expect("join thread");
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_pixmap_eq(label: &str, expected: &tiny_skia::Pixmap, actual: &tiny_skia::Pixmap) {
  assert_eq!(expected.width(), actual.width(), "{label}: width mismatch");
  assert_eq!(
    expected.height(),
    actual.height(),
    "{label}: height mismatch"
  );
  let expected_data = expected.data();
  let actual_data = actual.data();
  if expected_data == actual_data {
    return;
  }

  let width = expected.width() as usize;
  let height = expected.height() as usize;
  let mut first: Option<(usize, usize, [u8; 4], [u8; 4])> = None;
  let mut diff_min_x = usize::MAX;
  let mut diff_min_y = usize::MAX;
  let mut diff_max_x = 0usize;
  let mut diff_max_y = 0usize;
  let mut diff_pixels = 0usize;

  for y in 0..height {
    for x in 0..width {
      let idx = (y * width + x) * 4;
      let e = &expected_data[idx..idx + 4];
      let a = &actual_data[idx..idx + 4];
      if e == a {
        continue;
      }
      diff_pixels += 1;
      diff_min_x = diff_min_x.min(x);
      diff_min_y = diff_min_y.min(y);
      diff_max_x = diff_max_x.max(x);
      diff_max_y = diff_max_y.max(y);
      if first.is_none() {
        first = Some((x, y, e.try_into().unwrap(), a.try_into().unwrap()));
      }
    }
  }

  if let Some((x, y, e, a)) = first {
    panic!(
      "{label}: {diff_pixels} pixels differ; diff_bbox=({diff_min_x},{diff_min_y})-({diff_max_x},{diff_max_y}); first at ({x},{y}) expected={e:?} actual={a:?}"
    );
  }
  panic!("{label}: pixmaps differ, but could not locate mismatch");
}

fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let dom = renderer.parse_html(html).expect("parsed");
  let tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");
  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();
  let viewport = tree.viewport_size();

  let build_for_root = |root: &FragmentNode| -> DisplayList {
    DisplayListBuilder::with_image_cache(image_cache.clone())
      .with_font_context(font_ctx.clone())
      .with_svg_filter_defs(tree.svg_filter_defs.clone())
      .with_svg_id_defs(tree.svg_id_defs.clone())
      .with_scroll_state(ScrollState::default())
      .with_device_pixel_ratio(1.0)
      // Keep display-list building deterministic; these tests focus on renderer tiling.
      .with_parallelism(&PaintParallelism::disabled())
      .with_viewport_size(viewport.width, viewport.height)
      .build_with_stacking_tree_offset_checked(root, Point::ZERO)
      .expect("display list")
  };

  let mut list = build_for_root(&tree.root);
  for extra in &tree.additional_fragments {
    list.append(build_for_root(extra));
  }
  (list, font_ctx)
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
    used_dark_color_scheme: false,
    forced_colors: false,
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
fn backdrop_filter_clips_to_border_radius() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        border-radius: 20px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="overlay"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 128, 128);
  let pixmap = DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Outside the rounded corner (top-left pixel) remains red.
  assert_eq!(pixel(&pixmap, 0, 0), (255, 0, 0, 255));
  // Inside the rounded rect, the red backdrop is inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 35, 20), (0, 255, 255, 255));
  // Outside the element bounds remains untouched.
  assert_eq!(pixel(&pixmap, 60, 60), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_is_masked_by_mask_image() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);

        /* Mask out the left half of the overlay. */
        mask-image: linear-gradient(90deg, transparent 0 50%, black 50% 100%);
        -webkit-mask-image: linear-gradient(90deg, transparent 0 50%, black 50% 100%);
        mask-repeat: no-repeat;
        -webkit-mask-repeat: no-repeat;
        mask-size: 100% 100%;
        -webkit-mask-size: 100% 100%;
      }
    </style>
    <div id="bg"></div>
    <div id="overlay"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 128, 128);
  let pixmap = DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Masked-out half: stays red (backdrop was not mutated outside the compositing group).
  assert_eq!(pixel(&pixmap, 10, 20), (255, 0, 0, 255));
  // Masked-in half: red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 30, 20), (0, 255, 255, 255));
  // Outside overlay: stays red.
  assert_eq!(pixel(&pixmap, 60, 60), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_clips_with_affine_transform() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #overlay {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        border-radius: 8px;
        backdrop-filter: invert(1);
        transform: rotate(45deg);
      }
    </style>
    <div id="bg"></div>
    <div id="overlay"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 128, 128);
  let pixmap = DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Inside the overlay's AABB but outside the rotated rounded rect: should remain red.
  assert_eq!(pixel(&pixmap, 60, 16), (255, 0, 0, 255));
  // Inside the rotated rounded rect: red backdrop is inverted to cyan.
  assert_eq!(pixel(&pixmap, 40, 40), (0, 255, 255, 255));
}

#[test]
fn filter_layer_clips_with_affine_transform() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #overlay {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        border-radius: 14px;
        filter: invert(1);
        transform: rotate(45deg);
      }
      #fill {
        position: absolute;
        inset: 0;
        background: rgb(0 0 0);
      }
    </style>
    <div id="bg"></div>
    <div id="overlay"><div id="fill"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 128, 128);
  let pixmap = DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Inside the unrounded rotated quad but outside the rotated rounded rect: should remain red.
  assert_eq!(pixel(&pixmap, 40, 12), (255, 0, 0, 255));
  // Interior pixels from the black fill are inverted to white.
  assert_eq!(pixel(&pixmap, 40, 40), (255, 255, 255, 255));
}

#[test]
fn filter_drop_shadow_preserves_outsets_with_affine_transform() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #overlay {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        border-radius: 10px;
        background: rgb(0 255 0);
        transform: rotate(45deg);
        filter: drop-shadow(60px 0 0 rgb(0 0 0));
      }
    </style>
    <div id="bg"></div>
    <div id="overlay"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 200, 128);
  let pixmap = DisplayListRenderer::new(200, 128, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The drop-shadow offset is specified in the element's local coordinate system and therefore
  // rotates with the element. With a 45deg rotation, `drop-shadow(60px 0)` offsets the shadow
  // down+right by ~42px.
  //
  // Sample a pixel well inside the expected shadow region to ensure filter outsets are preserved
  // and not clipped away by the transformed stacking context.
  assert_eq!(pixel(&pixmap, 82, 82), (0, 0, 0, 255));
  // Background remains red away from the shadow.
  assert_eq!(pixel(&pixmap, 180, 10), (255, 0, 0, 255));
}

#[test]
fn parallel_paint_is_used_with_backdrop_filter_and_matches_serial() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      .left {
        position: absolute;
        left: 0;
        top: 0;
        width: 64px;
        height: 64px;
        background: rgb(255 0 0);
      }
      .right {
        position: absolute;
        left: 64px;
        top: 0;
        width: 64px;
        height: 64px;
        background: rgb(0 0 255);
      }
      #overlay {
        position: absolute;
        left: 32px;
        top: 0;
        width: 64px;
        height: 64px;
        backdrop-filter: blur(6px);
      }
    </style>
    <div class="left"></div>
    <div class="right"></div>
    <div id="overlay"></div>
  "#;

  let serial_pool = ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .expect("rayon pool");
  let (list, font_ctx, serial) = serial_pool.install(|| {
    let (list, font_ctx) = build_display_list(html, 128, 64);
    let serial = DisplayListRenderer::new(128, 64, Rgba::WHITE, font_ctx.clone())
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render(&list)
      .expect("serial render");
    (list, font_ctx, serial)
  });

  let parallelism = PaintParallelism {
    tile_size: 32,
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(128, 64, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  let cpu_budget = fastrender::system::cpu_budget();
  if cpu_budget > 1 {
    assert!(
      report.parallel_used,
      "expected backdrop-filter scene to use parallel tiling (fallback={:?})",
      report.fallback_reason
    );
    assert!(report.tiles > 1, "expected multiple tiles to be rendered");
  }
  assert_pixmap_eq(
    "parallel backdrop-filter output diverged from serial",
    &serial,
    &report.pixmap,
  );
}

#[test]
fn parallel_backdrop_filter_with_large_box_shadow_matches_serial() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: linear-gradient(180deg, #f5f7fb 0%, #eef2ff 100%); }
      #panel {
        position: absolute;
        left: 50px;
        top: 50px;
        width: 500px;
        height: 340px;
        border-radius: 12px;
        background: white;
        box-shadow: 0 20px 40px rgba(15, 23, 42, 0.2);
      }
      #stripe {
        position: absolute;
        left: 0;
        top: 0;
        width: 600px;
        height: 480px;
        background: linear-gradient(90deg, red 0 25%, green 25% 50%, blue 50% 75%, yellow 75% 100%);
        opacity: 0.35;
      }
      #overlay {
        position: absolute;
        left: 120px;
        top: 120px;
        width: 360px;
        height: 200px;
        border-radius: 14px;
        background: rgba(255, 255, 255, 0.55);
        backdrop-filter: blur(14px) saturate(1.1);
      }
    </style>
    <div id="stripe"></div>
    <div id="panel"></div>
    <div id="overlay"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 600, 480);
  let serial = DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 256,
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  let cpu_budget = fastrender::system::cpu_budget();
  if cpu_budget > 1 {
    assert!(report.parallel_used, "expected multiple tiles");
  }
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
}

#[test]
fn parallel_backdrop_filter_with_mask_and_radii_matches_serial() {
  let mut list = DisplayList::new();
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, 128.0, 128.0),
    color: Rgba::WHITE,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, 64.0, 128.0),
    color: Rgba::new(255, 0, 0, 1.0),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(64.0, 0.0, 64.0, 128.0),
    color: Rgba::new(0, 0, 255, 1.0),
  }));

  let bounds = Rect::from_xywh(20.0, 12.0, 88.0, 96.0);
  let sc = StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: true,
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
    backdrop_filters: vec![ResolvedFilter::Blur(6.0), ResolvedFilter::Invert(1.0)],
    radii: BorderRadii::new(
      BorderRadius::uniform(18.0),
      BorderRadius::uniform(10.0),
      BorderRadius::uniform(22.0),
      BorderRadius::uniform(6.0),
    ),
    mask: Some(patterned_mask(bounds)),
    has_clip_path: false,
  };
  list.push(DisplayItem::PushStackingContext(sc));
  // Add content so both the mask and border radii clipping paths are exercised, while the
  // backdrop-filter chain still has visible backdrop variation to blur/invert.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::from_rgba8(0, 0, 0, 96),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(bounds.x() + 6.0, bounds.y() + 22.0, 48.0, 40.0),
    color: Rgba::from_rgba8(0, 255, 0, 160),
  }));
  list.push(DisplayItem::PopStackingContext);

  let font_ctx = FontContext::new();
  let serial = DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 32,
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(128, 128, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  assert!(
    report.parallel_used,
    "expected masked backdrop-filter scene to use parallel tiling (fallback={:?})",
    report.fallback_reason
  );
  assert!(report.tiles > 1, "expected multiple tiles to be rendered");
  assert_pixmap_eq(
    "parallel masked backdrop-filter output diverged from serial",
    &serial,
    &report.pixmap,
  );
}

#[test]
fn parallel_drop_shadow_and_clip_path_cross_tile_matches_serial() {
  // This mirrors the structure of the `filter_backdrop_layers` fixture, but is small enough to
  // run quickly as a unit test. The drop-shadow filtered element is positioned so its shadow
  // crosses the tile boundary at x=512.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: linear-gradient(180deg, #f5f7fb 0%, #eef2ff 100%); }
      #card {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 536px;
        height: 360px;
        border-radius: 12px;
        overflow: hidden;
        background: radial-gradient(circle at 20% 20%, rgba(37, 99, 235, 0.18), transparent 45%),
          radial-gradient(circle at 70% 60%, rgba(217, 70, 239, 0.12), transparent 52%);
        box-shadow: 0 8px 18px rgba(17, 24, 39, 0.06);
      }
      #shadowed {
        position: absolute;
        left: 400px;
        top: 60px;
        width: 160px;
        height: 160px;
        background: linear-gradient(135deg, #2563eb, #d946ef);
        filter: drop-shadow(0 18px 22px rgba(37, 99, 235, 0.3));
        clip-path: polygon(12% 0%, 100% 6%, 90% 100%, 0% 94%);
      }
      #glass {
        position: absolute;
        left: 60px;
        top: 140px;
        width: 360px;
        height: 180px;
        border-radius: 14px;
        background: rgba(255, 255, 255, 0.55);
        backdrop-filter: blur(14px) saturate(1.1);
      }
    </style>
    <div id="card">
      <div id="shadowed"></div>
      <div id="glass"></div>
    </div>
  "#;

  let (list, font_ctx) = build_display_list(html, 600, 480);
  let serial = DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 512,
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  let cpu_budget = fastrender::system::cpu_budget();
  if cpu_budget > 1 {
    assert!(
      report.parallel_used,
      "expected scene to use parallel tiling (fallback={:?})",
      report.fallback_reason
    );
    assert!(report.tiles > 1, "expected multiple tiles");
  }
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
}

#[test]
fn parallel_mask_image_url_with_drop_shadow_matches_serial() {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let mask_bytes =
    fs::read(manifest_dir.join("tests/pages/fixtures/assets/images/mask.png")).expect("mask png");
  let pattern_bytes = fs::read(manifest_dir.join("tests/pages/fixtures/assets/images/pattern.png"))
    .expect("pattern png");
  let mask_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(mask_bytes)
  );
  let pattern_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(pattern_bytes)
  );

  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: linear-gradient(180deg, #f5f7fb 0%, #eef2ff 100%); }}
        #card {{
          position: absolute;
          left: 32px;
          top: 32px;
          width: 536px;
          height: 360px;
          border-radius: 12px;
          overflow: hidden;
          background:
            radial-gradient(circle at 20% 20%, rgba(37, 99, 235, 0.18), transparent 45%),
            radial-gradient(circle at 70% 60%, rgba(217, 70, 239, 0.12), transparent 52%),
            url("{pattern_url}");
          background-size: cover;
          box-shadow: 0 8px 18px rgba(17, 24, 39, 0.06);
        }}
        #glow {{
          position: absolute;
          inset: -20%;
          background: radial-gradient(circle, rgba(255, 255, 255, 0.32), transparent 50%);
          filter: blur(30px);
          z-index: 0;
        }}
        #shadowed {{
          position: absolute;
          left: 400px;
          top: 60px;
          width: 160px;
          height: 160px;
          background: linear-gradient(135deg, #2563eb, #d946ef);
          mask-image: url("{mask_url}");
          mask-size: cover;
          mask-repeat: no-repeat;
          filter: drop-shadow(0 18px 22px rgba(37, 99, 235, 0.3));
          clip-path: polygon(12% 0%, 100% 6%, 90% 100%, 0% 94%);
          z-index: 1;
        }}
        #glass {{
          position: absolute;
          left: 60px;
          top: 140px;
          width: 360px;
          height: 180px;
          border-radius: 14px;
          background: rgba(255, 255, 255, 0.55);
          backdrop-filter: blur(14px) saturate(1.1);
          z-index: 1;
        }}
      </style>
      <div id="card">
        <div id="glow"></div>
        <div id="shadowed"></div>
        <div id="glass"></div>
      </div>
    "#
  );

  let (list, font_ctx) = build_display_list(&html, 600, 480);
  let serial = DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("serial render");

  let parallelism = PaintParallelism {
    tile_size: 512,
    ..PaintParallelism::enabled()
  };
  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  let cpu_budget = fastrender::system::cpu_budget();
  if cpu_budget > 1 {
    assert!(report.parallel_used, "expected multiple tiles");
  }
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
}

#[test]
fn parallel_filter_backdrop_layers_fixture_matches_serial() {
  run_with_large_stack(|| {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_root = manifest_dir.join("tests/pages/fixtures/filter_backdrop_layers");
    let html_path = fixture_root.join("index.html");
    let html = fs::read_to_string(&html_path).expect("read fixture html");
    let canonical_path = html_path.canonicalize().expect("canonical fixture path");
    let base_url = Url::from_file_path(&canonical_path)
      .expect("fixture path should be convertible to file URL")
      .to_string();

    // Build the display list via the full HTML+stylesheet pipeline so relative resources resolve
    // exactly as they do in `render_fixtures`.
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    )]));
    let mut renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("renderer");
    let options = RenderOptions::new()
      .with_viewport(600, 480)
      .with_diagnostics_level(DiagnosticsLevel::None)
      .with_runtime_toggles(toggles)
      .with_paint_parallelism(PaintParallelism::disabled());
    let artifacts = RenderArtifactRequest {
      display_list: true,
      ..RenderArtifactRequest::none()
    };
    let report = renderer
      .render_html_with_stylesheets_report(&html, &base_url, options, artifacts)
      .expect("serial fixture render");
    let font_ctx = renderer.font_context().clone();
    let list = report
      .artifacts
      .display_list
      .expect("display list artifact");

    let serial = DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx.clone())
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render(&list)
      .expect("serial display-list render");

    let parallelism = PaintParallelism {
      tile_size: 512,
      ..PaintParallelism::enabled()
    };
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .stack_size(8 * 1024 * 1024)
      .build()
      .expect("rayon pool");
    let report = pool.install(|| {
      DisplayListRenderer::new(600, 480, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(parallelism)
        .render_with_report(&list)
        .expect("parallel render")
    });

    let cpu_budget = fastrender::system::cpu_budget();
    if cpu_budget > 1 {
      assert!(
        report.parallel_used,
        "expected fixture to use parallel tiling (fallback={:?})",
        report.fallback_reason
      );
      assert!(report.tiles > 1, "expected multiple tiles");
    }
    assert_pixmap_eq("fixture output diverged", &serial, &report.pixmap);
  });
}
