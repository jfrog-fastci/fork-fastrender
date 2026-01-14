use crate::debug::runtime::RuntimeToggles;
use crate::geometry::Rect;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list::DisplayList;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use crate::style::color::Rgba;
use crate::{DisplayListOptimizer, FastRender, FontContext, RenderArtifactRequest, RenderOptions};
use std::collections::HashMap;
use tiny_skia::{Color, Pixmap};

#[derive(Debug, Clone, Copy)]
struct RectU32 {
  x: u32,
  y: u32,
  w: u32,
  h: u32,
}

#[derive(Debug)]
struct RegionPatch {
  /// Pixmap covering the halo-expanded `render_rect`.
  pixmap: Pixmap,
  /// Patch origin in the full-frame coordinate space.
  origin: (u32, u32),
}

fn build_optimized_display_list(html: &str, width: u32, height: u32) -> DisplayList {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));

  let mut renderer = FastRender::new().expect("renderer should construct");
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_device_pixel_ratio(1.0)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_runtime_toggles(toggles);
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      "https://example.invalid/",
      options,
      RenderArtifactRequest {
        display_list: true,
        ..Default::default()
      },
    )
    .expect("render should succeed");
  let display_list = report
    .artifacts
    .display_list
    .expect("display list should be captured");

  // Mirror the production paint pipeline: optimize the list before rasterization.
  let viewport = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let (optimized, _) = DisplayListOptimizer::new()
    .optimize_checked(&display_list, viewport)
    .expect("display list optimization should succeed");
  optimized
}

fn render_full_and_tile_halo_px(
  list: &DisplayList,
  width: u32,
  height: u32,
  background: Rgba,
) -> (Pixmap, u32) {
  let mut renderer = DisplayListRenderer::new(width, height, background, FontContext::new())
    .expect("renderer should construct")
    .with_parallelism(PaintParallelism::disabled());
  let halo_px = renderer
    .tile_halo_px(list)
    .expect("tile halo should compute");
  let pixmap = renderer.render(list).expect("full render should succeed");
  (pixmap, halo_px)
}

fn render_region_patch(
  list: &DisplayList,
  full_size: (u32, u32),
  background: Rgba,
  damage_rect: RectU32,
  halo_px: u32,
) -> RegionPatch {
  let (full_w, full_h) = full_size;

  let render_x0 = damage_rect.x.saturating_sub(halo_px);
  let render_y0 = damage_rect.y.saturating_sub(halo_px);
  let render_x1 = damage_rect
    .x
    .saturating_add(damage_rect.w)
    .saturating_add(halo_px)
    .min(full_w);
  let render_y1 = damage_rect
    .y
    .saturating_add(damage_rect.h)
    .saturating_add(halo_px)
    .min(full_h);

  let render_w = render_x1.saturating_sub(render_x0).max(1);
  let render_h = render_y1.saturating_sub(render_y0).max(1);

  let render_css_rect = Rect::from_xywh(
    render_x0 as f32,
    render_y0 as f32,
    render_w as f32,
    render_h as f32,
  );
  let view = DisplayListOptimizer::new()
    .intersect_view(list.items(), render_css_rect)
    .expect("should build intersected display list view");

  let mut renderer = DisplayListRenderer::new(render_w, render_h, background, FontContext::new())
    .expect("patch renderer should construct")
    .with_parallelism(PaintParallelism::disabled());

  if render_x0 > 0 || render_y0 > 0 {
    renderer
      .canvas
      .translate(-(render_x0 as f32), -(render_y0 as f32));
  }
  renderer
    .render_slice(&view)
    .expect("patch render should succeed");
  RegionPatch {
    pixmap: renderer.canvas.into_pixmap(),
    origin: (render_x0, render_y0),
  }
}

fn fill_pixmap(pixmap: &mut Pixmap, color: Rgba) {
  pixmap.fill(Color::from_rgba8(
    color.r,
    color.g,
    color.b,
    (color.a * 255.0) as u8,
  ));
}

fn composite_damage_rect_into_full(full: &mut Pixmap, patch: &RegionPatch, damage_rect: RectU32) {
  let full_w = full.width();
  let patch_w = patch.pixmap.width();

  let src_x = damage_rect.x.saturating_sub(patch.origin.0).min(patch_w);
  let src_y = damage_rect
    .y
    .saturating_sub(patch.origin.1)
    .min(patch.pixmap.height());

  let copy_w = damage_rect.w.min(patch_w.saturating_sub(src_x));
  let copy_h = damage_rect
    .h
    .min(patch.pixmap.height().saturating_sub(src_y));
  assert_eq!(
    copy_w, damage_rect.w,
    "patch pixmap should fully cover damage rect horizontally (damage={damage_rect:?}, patch_origin={:?}, patch_w={patch_w})",
    patch.origin
  );
  assert_eq!(
    copy_h, damage_rect.h,
    "patch pixmap should fully cover damage rect vertically (damage={damage_rect:?}, patch_origin={:?}, patch_h={})",
    patch.origin,
    patch.pixmap.height()
  );

  let dst_data = full.data_mut();
  let src_data = patch.pixmap.data();
  let bytes_per_row = (damage_rect.w * 4) as usize;

  for row in 0..damage_rect.h {
    let dst_y = damage_rect.y + row;
    let src_y = src_y + row;
    let dst_idx = ((dst_y * full_w + damage_rect.x) * 4) as usize;
    let src_idx = ((src_y * patch_w + src_x) * 4) as usize;
    dst_data[dst_idx..dst_idx + bytes_per_row]
      .copy_from_slice(&src_data[src_idx..src_idx + bytes_per_row]);
  }
}

fn assert_pixmap_region_eq(expected: &Pixmap, actual: &Pixmap, rect: RectU32) {
  assert_eq!(
    (expected.width(), expected.height()),
    (actual.width(), actual.height()),
    "pixmap sizes must match for region compare"
  );
  let w = expected.width();

  let mut mismatch_pixels = 0u32;
  let mut first_mismatch = None;

  for y in rect.y..rect.y + rect.h {
    for x in rect.x..rect.x + rect.w {
      let idx = ((y * w + x) * 4) as usize;
      let e = &expected.data()[idx..idx + 4];
      let a = &actual.data()[idx..idx + 4];
      if e != a {
        mismatch_pixels += 1;
        if first_mismatch.is_none() {
          first_mismatch = Some((x, y, [e[0], e[1], e[2], e[3]], [a[0], a[1], a[2], a[3]]));
        }
      }
    }
  }

  if mismatch_pixels > 0 {
    let (x, y, e, a) = first_mismatch.expect("first mismatch present");
    panic!(
      "region pixels differ: rect={rect:?} mismatch_pixels={mismatch_pixels} first=({x},{y}) expected_rgba={e:?} actual_rgba={a:?}"
    );
  }
}

#[test]
fn display_list_region_paint_filter_blur_matches_full_render_crop() {
  const WIDTH: u32 = 128;
  const HEIGHT: u32 = 128;
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #blur {
        position: absolute;
        left: 76px;
        top: 48px;
        width: 40px;
        height: 32px;
        filter: blur(6px);
      }
      #blur .left {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 32px;
        background: rgb(255, 0, 0);
      }
      #blur .right {
        position: absolute;
        left: 20px;
        top: 0;
        width: 20px;
        height: 32px;
        background: rgb(0, 0, 255);
      }
    </style>
    <div id="blur"><div class="left"></div><div class="right"></div></div>
  "#;

  let list = build_optimized_display_list(html, WIDTH, HEIGHT);
  let has_blur_filter = list.items().iter().any(|item| {
    matches!(item, DisplayItem::PushStackingContext(sc) if sc
      .filters
      .iter()
      .any(|filter| matches!(filter, ResolvedFilter::Blur(radius) if *radius > 0.0)))
  });
  assert!(
    has_blur_filter,
    "expected display list to contain a blur filter stacking context"
  );

  let (full, halo_px) = render_full_and_tile_halo_px(&list, WIDTH, HEIGHT, Rgba::WHITE);
  assert!(halo_px > 0, "expected non-zero tile halo for blur content");

  let damage_rect = RectU32 {
    x: 32,
    y: 32,
    w: 64,
    h: 64,
  };
  let patch = render_region_patch(&list, (WIDTH, HEIGHT), Rgba::WHITE, damage_rect, halo_px);

  let mut composite = Pixmap::new(WIDTH, HEIGHT).expect("composite pixmap");
  fill_pixmap(&mut composite, Rgba::WHITE);
  composite_damage_rect_into_full(&mut composite, &patch, damage_rect);

  assert_pixmap_region_eq(&full, &composite, damage_rect);
}

#[test]
fn display_list_region_paint_backdrop_filter_blur_matches_full_render_crop() {
  const WIDTH: u32 = 128;
  const HEIGHT: u32 = 128;
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #bar {
        position: absolute;
        left: 0;
        top: 0;
        width: 96px;
        height: 128px;
        background: black;
      }
      #overlay {
        position: absolute;
        left: 80px;
        top: 48px;
        width: 40px;
        height: 32px;
        backdrop-filter: blur(6px);
      }
    </style>
    <div id="bar"></div>
    <div id="overlay"></div>
  "#;

  let list = build_optimized_display_list(html, WIDTH, HEIGHT);
  let has_backdrop_blur = list.items().iter().any(|item| {
    matches!(item, DisplayItem::PushStackingContext(sc) if sc
      .backdrop_filters
      .iter()
      .any(|filter| matches!(filter, ResolvedFilter::Blur(radius) if *radius > 0.0)))
  });
  assert!(
    has_backdrop_blur,
    "expected display list to contain a backdrop-filter blur stacking context"
  );

  let (full, halo_px) = render_full_and_tile_halo_px(&list, WIDTH, HEIGHT, Rgba::WHITE);
  assert!(
    halo_px > 0,
    "expected non-zero tile halo for backdrop-filter blur content"
  );

  let damage_rect = RectU32 {
    x: 32,
    y: 32,
    w: 64,
    h: 64,
  };
  let patch = render_region_patch(&list, (WIDTH, HEIGHT), Rgba::WHITE, damage_rect, halo_px);

  let mut composite = Pixmap::new(WIDTH, HEIGHT).expect("composite pixmap");
  fill_pixmap(&mut composite, Rgba::WHITE);
  composite_damage_rect_into_full(&mut composite, &patch, damage_rect);

  assert_pixmap_region_eq(&full, &composite, damage_rect);
}
