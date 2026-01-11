use super::util::create_stacking_context_bounds_renderer;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::{
  FastRender, FastRenderConfig, FontConfig, LayoutParallelism, RenderArtifactRequest,
  RenderArtifacts, RenderOptions, ResourcePolicy, Rgba,
};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use tiny_skia::Pixmap;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 100;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_is_white(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 240 && g > 240 && b > 240 && a > 240,
    "{msg}: expected white, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 50 && b < 50 && a > 240,
    "{msg}: expected red, got rgba=({r},{g},{b},{a})"
  );
}

#[derive(Clone, Copy, Debug)]
enum MaskPropertyFlavor {
  Standard,
  Webkit,
}

fn html_with_mask_clip(clip: &str, flavor: MaskPropertyFlavor) -> String {
  let (mask_image, mask_size, mask_repeat, mask_clip) = match flavor {
    MaskPropertyFlavor::Standard => ("mask-image", "mask-size", "mask-repeat", "mask-clip"),
    MaskPropertyFlavor::Webkit => (
      "-webkit-mask-image",
      "-webkit-mask-size",
      "-webkit-mask-repeat",
      "-webkit-mask-clip",
    ),
  };

  format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: white; }}
        #target {{
          width: {WIDTH}px;
          height: {HEIGHT}px;
          background: rgb(255, 0, 0);
          color: black;
          font-family: "DejaVu Sans Subset";
          font-size: 32px;
          line-height: 32px;
          {mask_image}: linear-gradient(#fff, #fff);
          {mask_size}: 100% 100%;
          {mask_repeat}: no-repeat;
          {mask_clip}: {clip};
        }}
      </style>
      <div id="target">Hello</div>
    "#
  )
}

#[test]
fn mask_clip_text_clips_mask_to_glyph_shapes() {
  for (label, flavor) in [
    ("standard", MaskPropertyFlavor::Standard),
    ("webkit", MaskPropertyFlavor::Webkit),
  ] {
    let html_text_clip = html_with_mask_clip("text", flavor);
    let html_content_clip = html_with_mask_clip("content-box", flavor);

    let mut renderer = create_stacking_context_bounds_renderer();
    let text_clip = renderer
      .render_html(&html_text_clip, WIDTH, HEIGHT)
      .unwrap_or_else(|_| panic!("{label}: render mask-clip:text"));
    assert_is_white(
      rgba_at(&text_clip, 10, 90),
      &format!("{label}: mask-clip:text should mask out the element outside glyph shapes"),
    );

    let mut renderer = create_stacking_context_bounds_renderer();
    let content_clip = renderer
      .render_html(&html_content_clip, WIDTH, HEIGHT)
      .unwrap_or_else(|_| panic!("{label}: render mask-clip:content-box"));
    assert_is_red(
      rgba_at(&content_clip, 10, 90),
      &format!("{label}: mask-clip:content-box should leave the element background visible"),
    );
  }
}

fn deterministic_toggles() -> RuntimeToggles {
  // Keep the captured display list stable. (This test only cares about paint tiling determinism.)
  RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
    ("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "0".to_string()),
  ]))
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
    }
  }

  if let Some((idx, a, b)) = first {
    let x = idx % (width as usize);
    let y = idx / (width as usize);
    panic!(
      "{label}: {mismatched_pixels} pixels differ; first at ({x}, {y}) expected={a:?} actual={b:?}"
    );
  }
  panic!("{label}: buffers differ");
}

#[test]
fn mask_clip_text_is_deterministic_under_parallel_tiling() {
  let html = html_with_mask_clip("text", MaskPropertyFlavor::Webkit);

  let config = FastRenderConfig::new()
    .with_runtime_toggles(deterministic_toggles())
    .with_default_viewport(WIDTH, HEIGHT)
    .with_font_sources(FontConfig::bundled_only())
    .with_resource_policy(ResourcePolicy::default().allow_http(false).allow_https(false))
    // Ensure we're only testing paint tiling; the captured display list should be stable.
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());

  let mut renderer = FastRender::with_config(config).expect("renderer");
  let options = RenderOptions::new().with_viewport(WIDTH, HEIGHT);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::default()
  });
  renderer
    .render_html_with_options_and_artifacts(&html, options, &mut artifacts)
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
  assert_is_white(
    rgba_at(&serial, 10, 90),
    "sanity: serial output should reflect mask-clip:text",
  );

  let parallelism = PaintParallelism {
    tile_size: 32,
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
