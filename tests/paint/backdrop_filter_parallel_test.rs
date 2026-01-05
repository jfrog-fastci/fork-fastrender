use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};
use base64::Engine;
use rayon::ThreadPoolBuilder;
use std::fs;
use std::path::PathBuf;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_pixmap_eq(label: &str, expected: &tiny_skia::Pixmap, actual: &tiny_skia::Pixmap) {
  assert_eq!(expected.width(), actual.width(), "{label}: width mismatch");
  assert_eq!(expected.height(), actual.height(), "{label}: height mismatch");
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

  let (list, font_ctx) = build_display_list(html, 128, 64);
  let serial = DisplayListRenderer::new(128, 64, Rgba::WHITE, font_ctx.clone())
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
    DisplayListRenderer::new(128, 64, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(parallelism)
      .render_with_report(&list)
      .expect("parallel render")
  });

  assert!(
    report.parallel_used,
    "expected backdrop-filter scene to use parallel tiling (fallback={:?})",
    report.fallback_reason
  );
  assert!(report.tiles > 1, "expected multiple tiles to be rendered");
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

  assert!(report.parallel_used, "expected multiple tiles");
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
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

  assert!(
    report.parallel_used,
    "expected scene to use parallel tiling (fallback={:?})",
    report.fallback_reason
  );
  assert!(report.tiles > 1, "expected multiple tiles");
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
}

#[test]
fn parallel_mask_image_url_with_drop_shadow_matches_serial() {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let mask_bytes = fs::read(manifest_dir.join("tests/pages/fixtures/assets/images/mask.png"))
    .expect("mask png");
  let pattern_bytes =
    fs::read(manifest_dir.join("tests/pages/fixtures/assets/images/pattern.png")).expect("pattern png");
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

  assert!(report.parallel_used, "expected multiple tiles");
  assert_pixmap_eq("parallel output diverged", &serial, &report.pixmap);
}
