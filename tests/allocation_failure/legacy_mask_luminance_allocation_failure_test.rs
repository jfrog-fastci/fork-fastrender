use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, RenderOptions};
use image::codecs::png::PngEncoder;
use image::ColorType;
use image::ImageEncoder;
use std::collections::HashMap;
use std::mem;
use super::{
  fail_nth_allocation, failed_allocs, lock_allocator, start_counting, stop_counting,
};

fn make_black_png_data_url(width: u32, height: u32) -> String {
  let img = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
  let mut bytes = Vec::new();
  PngEncoder::new(&mut bytes)
    .write_image(img.as_raw(), img.width(), img.height(), ColorType::Rgba8.into())
    .expect("encode png");
  format!("data:image/png;base64,{}", STANDARD.encode(bytes))
}

#[test]
fn legacy_mask_luminance_conversion_survives_allocation_failure() {
  let _guard = lock_allocator();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mask_w = 10_001u32;
  let mask_h = 1u32;
  let mask_url = make_black_png_data_url(mask_w, mask_h);
  let html = format!(
    r#"<!doctype html>
<style>
  html, body {{ margin: 0; }}
  .box {{
    width: 1px;
    height: 1px;
    background: rgb(255, 0, 0);
    mask-image: url("{mask_url}");
    mask-mode: luminance;
    mask-repeat: no-repeat;
  }}
</style>
<div class="box"></div>
"#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(1, 1)
    .with_runtime_toggles(toggles);

  let mask_bytes = (mask_w as usize) * (mask_h as usize) * 4;
  let align = mem::align_of::<u8>();

  start_counting(mask_bytes, align);
  let pixmap = renderer
    .render_html_with_options(&html, options)
    .expect("render legacy luminance mask for allocation counting");
  let matches = stop_counting();
  assert!(
    matches >= 2,
    "expected at least two allocations of {mask_bytes} bytes (decoded pixmap + conversion buffer), got {matches}"
  );

  // Without allocation failures, the black luminance mask should hide the red box.
  assert_eq!(
    &pixmap.data()[..4],
    &[255, 255, 255, 255],
    "expected mask conversion to succeed without allocation failures"
  );

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(1, 1)
    .with_runtime_toggles(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_PAINT_BACKEND".to_string(),
      "legacy".to_string(),
    )])));

  let start_failures = failed_allocs();
  // Skip all but the final matching allocation so we reliably target the mask conversion buffer.
  fail_nth_allocation(mask_bytes, align, matches - 1);
  let pixmap = renderer
    .render_html_with_options(&html, options)
    .expect("render legacy luminance mask with failed alloc");
  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger mask conversion allocation failure"
  );
  assert_eq!(
    &pixmap.data()[..4],
    &[255, 0, 0, 255],
    "expected the mask layer to be skipped after allocation failure"
  );
}
