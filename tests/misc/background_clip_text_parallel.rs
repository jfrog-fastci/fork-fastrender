use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::{
  FastRender, FastRenderConfig, FontConfig, LayoutParallelism, RenderArtifactRequest,
  RenderArtifacts, RenderOptions, ResourcePolicy, Rgba,
};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;

fn deterministic_toggles() -> RuntimeToggles {
  let mut toggles = HashMap::new();
  // Keep the captured display list stable. (This test only cares about paint tiling.)
  toggles.insert("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "0".to_string());
  RuntimeToggles::from_map(toggles)
}

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

  let mut mismatched_pixels = 0usize;
  let mut first: Option<(usize, [u8; 4], [u8; 4])> = None;
  let mut min_x = usize::MAX;
  let mut min_y = usize::MAX;
  let mut max_x = 0usize;
  let mut max_y = 0usize;
  for (idx, (a, b)) in expected
    .chunks_exact(4)
    .zip(actual.chunks_exact(4))
    .enumerate()
  {
    let a = [a[0], a[1], a[2], a[3]];
    let b = [b[0], b[1], b[2], b[3]];
    if a != b {
      mismatched_pixels += 1;
      if first.is_none() {
        first = Some((idx, a, b));
      }
      let x = idx % (width as usize);
      let y = idx / (width as usize);
      min_x = min_x.min(x);
      min_y = min_y.min(y);
      max_x = max_x.max(x);
      max_y = max_y.max(y);
    }
  }

  if let Some((idx, a, b)) = first {
    let x = idx % (width as usize);
    let y = idx / (width as usize);
    panic!(
      "{label}: {mismatched_pixels} pixels differ; bounds=({min_x},{min_y})..=({max_x},{max_y}); first at ({x}, {y}) expected={a:?} actual={b:?}"
    );
  }
  panic!("{label}: buffers differ");
}

fn assert_has_white_pixel_within_nonwhite_bounds(width: u32, height: u32, pixmap: &[u8]) {
  let bg = [255, 255, 255, 255];

  let mut min_x = width as usize;
  let mut min_y = height as usize;
  let mut max_x = 0usize;
  let mut max_y = 0usize;
  let mut any_non_bg = false;

  for y in 0..height as usize {
    for x in 0..width as usize {
      let idx = (y * width as usize + x) * 4;
      let px = [
        pixmap[idx],
        pixmap[idx + 1],
        pixmap[idx + 2],
        pixmap[idx + 3],
      ];
      if px != bg {
        any_non_bg = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  assert!(
    any_non_bg,
    "expected background-clip:text to paint non-background pixels"
  );

  // Verify that the paint isn't a filled rectangle by ensuring there is at least one pure-white
  // pixel inside the bounds of non-white pixels. If `background-clip:text` were ignored, the
  // gradient background would fill the element's rectangle and there would be no such pixel.
  let mut any_bg_within_bounds = false;
  for y in min_y..=max_y {
    for x in min_x..=max_x {
      let idx = (y * width as usize + x) * 4;
      let px = [
        pixmap[idx],
        pixmap[idx + 1],
        pixmap[idx + 2],
        pixmap[idx + 3],
      ];
      if px == bg {
        any_bg_within_bounds = true;
        break;
      }
    }
    if any_bg_within_bounds {
      break;
    }
  }

  assert!(
    any_bg_within_bounds,
    "expected background-clip:text to clip to glyphs (no background-colored pixel found within non-background bounds)"
  );
}

#[test]
fn background_clip_text_parallel_matches_serial_output() {
  const WIDTH: u32 = 512;
  const HEIGHT: u32 = 256;

  let html = r#"
    <!doctype html>
    <meta charset="utf-8" />
    <style>
      @font-face {
        font-family: "DejaVu Sans Subset";
        src: url("tests/fixtures/fonts/DejaVuSans-subset.ttf") format("truetype");
      }

      html, body {
        margin: 0;
        padding: 0;
        background: white;
        overflow: hidden;
        width: 512px;
        height: 256px;
      }

      body {
        display: flex;
        align-items: center;
        justify-content: center;
      }

      .sample {
        display: inline-block;
        font-family: "DejaVu Sans Subset";
        font-size: 176px;
        line-height: 1;
        font-weight: 700;
        background: linear-gradient(
          90deg,
          rgb(255, 0, 0) 0%,
          rgb(255, 255, 0) 20%,
          rgb(0, 255, 0) 45%,
          rgb(0, 255, 255) 70%,
          rgb(0, 0, 255) 100%
        );
        background-clip: text;
        -webkit-background-clip: text;
        color: transparent;
        -webkit-text-fill-color: transparent;
      }
    </style>
    <div class="sample">O</div>
  "#;

  let config = FastRenderConfig::new()
    .with_runtime_toggles(deterministic_toggles())
    .with_default_viewport(WIDTH, HEIGHT)
    .with_font_sources(FontConfig::bundled_only())
    .with_resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());

  let mut renderer = FastRender::with_config(config).expect("renderer");
  let options = RenderOptions::new().with_viewport(WIDTH, HEIGHT);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::default()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html for display list capture");

  let display_list = artifacts
    .display_list
    .take()
    .expect("expected display list artifact");
  let font_ctx = renderer.font_context().clone();

  let serial = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
    .expect("serial renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&display_list)
    .expect("serial render");

  assert_has_white_pixel_within_nonwhite_bounds(WIDTH, HEIGHT, serial.data());

  let parallelism = PaintParallelism {
    tile_size: 64,
    log_timing: false,
    min_display_items: 1,
    min_tiles: 1,
    min_build_fragments: 1,
    build_chunk_size: 1,
    ..PaintParallelism::enabled()
  };

  let pool = ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("rayon pool");
  let report = pool.install(|| {
    DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx)
      .expect("parallel renderer")
      .with_parallelism(parallelism)
      .render_with_report(&display_list)
      .expect("parallel render")
  });

  assert!(report.parallel_used, "expected parallel tiling to be used");
  assert_rgba8888_pixels_eq(
    WIDTH,
    HEIGHT,
    serial.data(),
    report.pixmap.data(),
    "serial_vs_parallel",
  );
}
