use base64::Engine;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{
  FastRender, FastRenderConfig, FontConfig, LayoutParallelism, PaintParallelism, RenderOptions,
  ResourcePolicy,
};
use image::codecs::gif::GifEncoder;
use image::Delay;
use image::Frame;
use image::Rgba as ImageRgba;
use image::RgbaImage;
use std::collections::HashMap;

fn deterministic_renderer() -> FastRender {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    // Keep rendering deterministic and avoid rayon fan-out in CI/agent environments.
    ("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "0".to_string()),
    // Pin the paint backend so this test isn't affected by FASTR_PAINT_BACKEND.
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
  ]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_default_viewport(8, 8)
    // Keep font metrics hermetic (and avoid system font discovery).
    .with_font_sources(FontConfig::bundled_only())
    // Ensure the test never reaches the network.
    .with_resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    // Keep execution predictable for tiny documents.
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  FastRender::with_config(config).expect("create deterministic renderer")
}

#[test]
fn animated_gif_frame_selection_is_driven_by_render_options_animation_time() {
  // Create a 2-frame 2×2 GIF: red for frame 0, blue for frame 1.
  let mut gif_bytes = Vec::new();
  {
    let red = RgbaImage::from_pixel(2, 2, ImageRgba([255, 0, 0, 255]));
    let blue = RgbaImage::from_pixel(2, 2, ImageRgba([0, 0, 255, 255]));
    // 100ms per frame → 0ms selects frame 0, 150ms selects frame 1.
    let delay = Delay::from_numer_denom_ms(100, 1);

    let mut encoder = GifEncoder::new(&mut gif_bytes);
    encoder
      .encode_frame(Frame::from_parts(red, 0, 0, delay))
      .expect("encode gif frame 0");
    encoder
      .encode_frame(Frame::from_parts(blue, 0, 0, delay))
      .expect("encode gif frame 1");
  }

  let encoded = base64::engine::general_purpose::STANDARD.encode(&gif_bytes);
  let url = format!("data:image/gif;base64,{encoded}");
  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; background: rgb(255, 255, 255); }}
        img {{ display: block; width: 2px; height: 2px; }}
      </style>
      <img decoding="sync" src="{url}">
    "#
  );

  let mut renderer = deterministic_renderer();

  let pixmap0 = renderer
    .render_html_with_options(
      &html,
      RenderOptions::new()
        .with_viewport(8, 8)
        .with_device_pixel_ratio(1.0)
        .with_animation_time(0.0),
    )
    .expect("render at t=0ms");

  let pixmap1 = renderer
    .render_html_with_options(
      &html,
      RenderOptions::new()
        .with_viewport(8, 8)
        .with_device_pixel_ratio(1.0)
        .with_animation_time(150.0),
    )
    .expect("render at t=150ms");

  let px0 = pixmap0.pixel(0, 0).expect("pixel in bounds");
  let px1 = pixmap1.pixel(0, 0).expect("pixel in bounds");

  assert_eq!(
    (px0.red(), px0.green(), px0.blue(), px0.alpha()),
    (255, 0, 0, 255),
    "expected first GIF frame at t=0ms"
  );
  assert_eq!(
    (px1.red(), px1.green(), px1.blue(), px1.alpha()),
    (0, 0, 255, 255),
    "expected second GIF frame at t=150ms"
  );

  assert_ne!(
    pixmap0.data(),
    pixmap1.data(),
    "expected Pixmap pixel data to differ when animation_time selects a different GIF frame"
  );
}
