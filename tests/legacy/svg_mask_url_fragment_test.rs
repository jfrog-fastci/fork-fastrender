use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};
use base64::Engine;
use image::codecs::png::PngEncoder;
use image::ColorType;
use image::ImageEncoder;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn render(html: &str, backend: PaintBackend) -> tiny_skia::Pixmap {
  crate::rayon_test_util::init_rayon_for_tests(1);

  let viewport_w = 20;
  let viewport_h = 20;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, viewport_w, viewport_h)
    .expect("laid out");

  paint_tree_with_resources_scaled_offset_backend(
    &fragment_tree,
    viewport_w,
    viewport_h,
    Rgba::WHITE,
    renderer.font_context().clone(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    backend,
  )
  .expect("painted")
}

#[test]
fn legacy_svg_mask_url_fragment_matches_display_list() {
  let html = r#"<!doctype html>
  <style>
    html, body { margin: 0; padding: 0; background: white; }
    #box {
      width: 20px;
      height: 20px;
      background: rgb(255 0 0);
      mask-image: url(#m);
      mask-mode: alpha;
      mask-repeat: no-repeat;
      mask-size: 100% 100%;
      mask-position: 0 0;
    }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg xmlns="http://www.w3.org/2000/svg">
    <mask id="m" maskUnits="objectBoundingBox" maskContentUnits="objectBoundingBox">
      <rect x="0" y="0" width="0.5" height="1" fill="white" />
      <rect x="0.5" y="0" width="0.5" height="1" fill="black" />
    </mask>
  </svg>
  <div id="box"></div>
  "#;

  let legacy = render(html, PaintBackend::Legacy);
  let display_list = render(html, PaintBackend::DisplayList);

  assert_eq!(pixel(&legacy, 5, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&legacy, 15, 10), (255, 255, 255, 255));

  assert_eq!(pixel(&display_list, 5, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&display_list, 15, 10), (255, 255, 255, 255));

  assert_eq!(
    legacy.data(),
    display_list.data(),
    "legacy and display-list backends should match for url(#mask) SVG masks"
  );
}

#[test]
fn legacy_raster_mask_url_matches_display_list() {
  let mut pixels = Vec::with_capacity(20 * 20 * 3);
  for y in 0..20 {
    for x in 0..20 {
      let is_left = x < 10;
      let (r, g, b) = if is_left { (255u8, 255u8, 255u8) } else { (0, 0, 0) };
      pixels.extend_from_slice(&[r, g, b]);
    }
  }

  let mut png_bytes = Vec::new();
  PngEncoder::new(&mut png_bytes)
    .write_image(&pixels, 20, 20, ColorType::Rgb8.into())
    .expect("encode png");
  let encoded = base64::engine::general_purpose::STANDARD.encode(png_bytes);
  let data_url = format!("data:image/png;base64,{}", encoded);

  let html = format!(
    r#"<!doctype html>
    <style>
      html, body {{ margin: 0; padding: 0; background: white; }}
      #box {{
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url({data_url});
        mask-mode: match-source;
        mask-repeat: no-repeat;
        mask-size: 20px 20px;
        mask-position: 0 0;
      }}
    </style>
    <div id="box"></div>
  "#
  );

  let legacy = render(&html, PaintBackend::Legacy);
  let display_list = render(&html, PaintBackend::DisplayList);

  assert_eq!(pixel(&legacy, 5, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&legacy, 15, 10), (255, 255, 255, 255));

  assert_eq!(pixel(&display_list, 5, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&display_list, 15, 10), (255, 255, 255, 255));

  assert_eq!(
    legacy.data(),
    display_list.data(),
    "legacy and display-list backends should match for raster mask-image: url(...)"
  );
}
