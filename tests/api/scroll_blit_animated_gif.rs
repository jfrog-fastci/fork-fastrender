use base64::Engine;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::text::font_loader::FontContext;
use fastrender::{
  FastRender, FastRenderConfig, FontConfig, LayoutParallelism, PaintParallelism, Point,
  RenderArtifactRequest, RenderArtifacts, RenderOptions, ResourcePolicy, Result, Rgba,
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
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
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
fn scroll_blit_disabled_for_animated_gifs_when_animation_time_is_active() -> Result<()> {
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
        img {{ display: block; width: 8px; height: 8px; }}
        .spacer {{ width: 8px; height: 20px; background: rgb(0, 255, 0); }}
      </style>
      <img decoding="sync" src="{url}">
      <div class="spacer"></div>
    "#
  );

  let mut renderer = deterministic_renderer();
  let mut artifacts0 = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..Default::default()
  });
  let pixmap0 = renderer.render_html_with_options_and_artifacts(
    &html,
    RenderOptions::new()
      .with_viewport(8, 8)
      .with_device_pixel_ratio(1.0)
      .with_scroll(0.0, 0.0)
      .with_animation_time(0.0),
    &mut artifacts0,
  )?;
  let list0 = artifacts0
    .display_list
    .take()
    .expect("expected display list capture for t=0ms");

  let px0 = pixmap0.pixel(0, 0).expect("pixel in bounds");
  assert_eq!(
    (px0.red(), px0.green(), px0.blue(), px0.alpha()),
    (255, 0, 0, 255),
    "expected first GIF frame at t=0ms"
  );
  assert!(
    list0.has_gif_images(),
    "expected display list to report GIF image URLs"
  );
  assert!(
    list0.has_animation_time_dependent_images(),
    "expected display list to report animation_time-dependent images for GIF sampling"
  );

  // Render the next frame with a small scroll delta and a later animation time.
  let mut artifacts1 = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..Default::default()
  });
  let pixmap1_full = renderer.render_html_with_options_and_artifacts(
    &html,
    RenderOptions::new()
      .with_viewport(8, 8)
      .with_device_pixel_ratio(1.0)
      .with_scroll(0.0, 1.0)
      .with_animation_time(150.0),
    &mut artifacts1,
  )?;
  let list1 = artifacts1
    .display_list
    .take()
    .expect("expected display list capture for t=150ms");

  let px1 = pixmap1_full.pixel(0, 0).expect("pixel in bounds");
  assert_eq!(
    (px1.red(), px1.green(), px1.blue(), px1.alpha()),
    (0, 0, 255, 255),
    "expected second GIF frame at t=150ms"
  );
  assert!(list1.has_animation_time_dependent_images());

  // Attempt scroll blit using the previous pixmap and the new display list.
  // The optimization must be disabled because GIF pixels can change with animation_time.
  let report = DisplayListRenderer::new_from_existing_pixmap(pixmap0, Rgba::WHITE, FontContext::new())?
    .render_scroll_blit_with_report(&list1, Point::new(0.0, 1.0))?;

  assert!(
    !report.scroll_blit_used,
    "expected scroll blit to be disabled when animated GIF sampling can change pixels"
  );
  assert_eq!(
    report.fallback_reason.as_deref(),
    Some("animated GIFs / animation_time affects images; full repaint")
  );

  // The output must match a full repaint at the same time/scroll state.
  let expected = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())?
    .render(&list1)?;
  assert_eq!(report.pixmap.data(), expected.data());

  Ok(())
}

