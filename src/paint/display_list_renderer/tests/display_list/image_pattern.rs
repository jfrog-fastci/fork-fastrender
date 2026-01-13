#![cfg(test)]

use base64::Engine as _;
use crate::api::FastRender;
use crate::geometry::{Point, Rect};
use crate::image_loader::ImageCache;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::{DisplayItem, DisplayListOptimizer, FontContext, PaintParallelism};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder};

#[test]
fn repeating_background_emits_single_image_pattern_item() {
  // Build a 1x1 opaque red PNG and embed it as a data: URL.
  let mut png = Vec::new();
  let encoder = PngEncoder::new(&mut png);
  encoder
    .write_image(&[255u8, 0, 0, 255], 1, 1, ColorType::Rgba8.into())
    .expect("encode 1x1 PNG");
  let data_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(&png)
  );

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #box {{
            width: 32px;
            height: 32px;
            background-image: url("{data_url}");
            background-repeat: repeat;
          }}
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 32, 32)
    .expect("layout document");

  let image_cache = ImageCache::new();
  let list = DisplayListBuilder::with_image_cache(image_cache.clone())
    .build_with_stacking_tree(&fragments.root);

  let pattern_items = list
    .items()
    .iter()
    .filter(|item| matches!(item, DisplayItem::ImagePattern(_)))
    .count();
  let image_items = list
    .items()
    .iter()
    .filter(|item| matches!(item, DisplayItem::Image(_)))
    .count();

  assert_eq!(
    pattern_items, 1,
    "expected a single pattern item for repeating background (got {pattern_items})"
  );
  assert_eq!(
    image_items, 0,
    "expected no per-tile Image items for repeating background (got {image_items})"
  );
  assert!(
    list.len() < 200,
    "display list should stay O(1) for repeats (got {} items)",
    list.len()
  );

  // Ensure the optimizer preserves the item and the renderer produces non-empty output.
  let viewport = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
  let (optimized, _) = DisplayListOptimizer::new()
    .optimize_checked(&list, viewport)
    .unwrap();
  assert_eq!(
    optimized
      .items()
      .iter()
      .filter(|item| matches!(item, DisplayItem::ImagePattern(_)))
      .count(),
    1,
    "optimizer should preserve the pattern fill"
  );

  let pixmap = DisplayListRenderer::new(32, 32, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&optimized)
    .unwrap();
  let top_left = pixmap.pixel(0, 0).expect("pixel in bounds");
  assert!(
    top_left.red() > 200
      && top_left.green() < 50
      && top_left.blue() < 50
      && top_left.alpha() == 255,
    "expected non-white pixels from repeated background (got {top_left:?})"
  );

  // Parity check against the legacy painter (which still paints tiled backgrounds).
  let scroll_state = ScrollState::default();
  let legacy = paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    32,
    32,
    Rgba::WHITE,
    FontContext::new(),
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &scroll_state,
    PaintBackend::Legacy,
  )
  .unwrap();

  assert_eq!(
    pixmap.data(),
    legacy.data(),
    "pattern-based display list output should match legacy painter"
  );
}

#[test]
fn image_pattern_background_size_cover_renders_under_border_radius_clip() {
  // Regression for netflix.com fixture: a background-image with `background-size: cover`
  // (default `background-repeat: repeat`) should still paint when clipped by border-radius.
  //
  // The netflix top-10 posters are painted via `DisplayItem::ImagePattern` and were completely
  // missing in the output, leaving the placeholder background color visible.

  // Build a non-square PNG that will be downscaled with `cover`.
  // Solid red makes it easy to assert on output pixels.
  let img_w = 20u32;
  let img_h = 30u32;
  let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];
  for px in pixels.chunks_exact_mut(4) {
    px[0] = 255;
    px[1] = 0;
    px[2] = 0;
    px[3] = 255;
  }
  let mut png = Vec::new();
  let encoder = PngEncoder::new(&mut png);
  encoder
    .write_image(&pixels, img_w, img_h, ColorType::Rgba8.into())
    .expect("encode PNG");
  let data_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(&png)
  );

  // Choose a box with a different aspect ratio so `cover` produces a tile size that is
  // slightly larger than the box in one axis (like netflix posters).
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; background: rgb(255,255,255); }}
          #outer {{
            width: 24px;
            height: 24px;
            overflow: hidden;
            position: relative;
            margin: 0;
          }}
          #card {{
            position: relative;
            width: 14px;
            height: 20px;
            margin: 3.5px;
            background: rgb(35,35,35);
            border-radius: 4px;
          }}
          #placeholder {{
            position: absolute;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgb(35,35,35);
            border-radius: 4px;
            z-index: 1;
          }}
          #poster {{
            position: absolute;
            top: 0; left: 0; right: 0; bottom: 0;
            border-radius: 4px;
            background-image: url("{data_url}");
            background-size: cover;
            /* repeat is the default; leave it unspecified to match netflix */
            z-index: 2;
          }}
        </style>
      </head>
      <body>
        <div id="outer">
          <div id="card">
            <div id="placeholder"></div>
            <div id="poster"></div>
          </div>
        </div>
      </body>
    </html>
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 30, 30)
    .expect("layout document");

  let image_cache = ImageCache::new();
  let list =
    DisplayListBuilder::with_image_cache(image_cache).build_with_stacking_tree(&fragments.root);

  let viewport = Rect::from_xywh(0.0, 0.0, 30.0, 30.0);
  let (optimized, _) = DisplayListOptimizer::new()
    .optimize_checked(&list, viewport)
    .unwrap();

  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&optimized)
    .unwrap();

  // Sample a pixel well inside the rounded corners so we don't hit antialiasing.
  // Expect something red-ish (not the grey placeholder).
  let px = pixmap.pixel(10, 10).expect("pixel in bounds");
  assert!(
    px.red() > 200 && px.green() < 80 && px.blue() < 80 && px.alpha() == 255,
    "expected poster background to paint over placeholder (got {px:?})"
  );
}

#[test]
fn image_pattern_background_size_cover_renders_for_absolutely_positioned_span() {
  // Netflix's top-10 posters use an absolutely-positioned <span> with a background-image.
  // Ensure we paint backgrounds for abspos inline elements (which compute to block-level boxes).

  // Solid red PNG as data URL.
  let img_w = 20u32;
  let img_h = 30u32;
  let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];
  for px in pixels.chunks_exact_mut(4) {
    px[0] = 255;
    px[1] = 0;
    px[2] = 0;
    px[3] = 255;
  }
  let mut png = Vec::new();
  let encoder = PngEncoder::new(&mut png);
  encoder
    .write_image(&pixels, img_w, img_h, ColorType::Rgba8.into())
    .expect("encode PNG");
  let data_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(&png)
  );

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; background: rgb(255,255,255); }}
          #outer {{
            width: 24px;
            height: 24px;
            overflow: hidden;
            position: relative;
            margin: 0;
          }}
          #card {{
            position: relative;
            width: 14px;
            height: 20px;
            margin: 3.5px;
            background: rgb(35,35,35);
            border-radius: 4px;
          }}
          #placeholder {{
            position: absolute;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgb(35,35,35);
            border-radius: 4px;
            z-index: 1;
          }}
          #poster {{
            position: absolute;
            top: 0; left: 0; right: 0; bottom: 0;
            border-radius: 4px;
            background-image: url("{data_url}");
            background-size: cover;
            z-index: 2;
          }}
        </style>
      </head>
      <body>
        <div id="outer">
          <div id="card">
            <div id="placeholder"></div>
            <span id="poster"></span>
          </div>
        </div>
      </body>
    </html>
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 30, 30)
    .expect("layout document");

  let image_cache = ImageCache::new();
  let list =
    DisplayListBuilder::with_image_cache(image_cache).build_with_stacking_tree(&fragments.root);

  let viewport = Rect::from_xywh(0.0, 0.0, 30.0, 30.0);
  let (optimized, _) = DisplayListOptimizer::new()
    .optimize_checked(&list, viewport)
    .unwrap();

  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&optimized)
    .unwrap();

  let px = pixmap.pixel(10, 10).expect("pixel in bounds");
  assert!(
    px.red() > 200 && px.green() < 80 && px.blue() < 80 && px.alpha() == 255,
    "expected poster background to paint over placeholder (got {px:?})"
  );
}

#[test]
fn image_pattern_background_paints_inside_overflow_scroll_container() {
  // Netflix's top-10 row is an overflow-x: scroll container with repeated background images.
  // Ensure scroll container clipping doesn't accidentally drop ImagePattern paints.

  // Solid red PNG as data URL.
  let img_w = 20u32;
  let img_h = 30u32;
  let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];
  for px in pixels.chunks_exact_mut(4) {
    px[0] = 255;
    px[1] = 0;
    px[2] = 0;
    px[3] = 255;
  }
  let mut png = Vec::new();
  let encoder = PngEncoder::new(&mut png);
  encoder
    .write_image(&pixels, img_w, img_h, ColorType::Rgba8.into())
    .expect("encode PNG");
  let data_url = format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(&png)
  );

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; background: rgb(255,255,255); }}
          #scroller {{
            width: 30px;
            height: 30px;
            overflow-x: scroll;
            overflow-y: visible;
            display: flex;
            padding: 0;
            margin: 0;
            list-style: none;
          }}
          li {{
            flex-shrink: 0;
            width: 20px;
            height: 20px;
            margin: 5px;
            position: relative;
            background: rgb(35,35,35);
            border-radius: 4px;
          }}
          .poster {{
            position: absolute;
            top: 0; left: 0; right: 0; bottom: 0;
            border-radius: 4px;
            background-image: url("{data_url}");
            background-size: cover;
          }}
        </style>
      </head>
      <body>
        <ul id="scroller">
          <li><span class="poster"></span></li>
          <li><span class="poster"></span></li>
          <li><span class="poster"></span></li>
        </ul>
      </body>
    </html>
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 30, 30)
    .expect("layout document");

  let image_cache = ImageCache::new();
  let list =
    DisplayListBuilder::with_image_cache(image_cache).build_with_stacking_tree(&fragments.root);

  let viewport = Rect::from_xywh(0.0, 0.0, 30.0, 30.0);
  let (optimized, _) = DisplayListOptimizer::new()
    .optimize_checked(&list, viewport)
    .unwrap();

  let unopt_pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();
  let opt_pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&optimized)
    .unwrap();

  // Sample a pixel in the first poster box (roughly center).
  let unopt_px = unopt_pixmap.pixel(10, 10).expect("pixel in bounds");
  let opt_px = opt_pixmap.pixel(10, 10).expect("pixel in bounds");
  assert!(
    opt_px.red() > 200 && opt_px.green() < 80 && opt_px.blue() < 80 && opt_px.alpha() == 255,
    "expected poster background to paint (unoptimized={unopt_px:?}, optimized={opt_px:?})"
  );
}
