use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::geometry::Point;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::style::color::Rgba;
use fastrender::{FastRender, Pixmap};
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

fn solid_color_png(width: u32, height: u32, rgba: [u8; 4]) -> String {
  let mut buf = Vec::new();
  let pixels: Vec<u8> = std::iter::repeat(rgba)
    .take((width * height) as usize)
    .flatten()
    .collect();

  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
    .expect("encode png");

  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

fn render_with_backend(
  html: &str,
  width: u32,
  height: u32,
  scale: f32,
  backend: PaintBackend,
) -> Pixmap {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, width, height)
    .expect("layout document");

  paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    width,
    height,
    Rgba::RED,
    renderer.font_context().clone(),
    ImageCache::new(),
    scale,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    backend,
  )
  .expect("paint")
}

#[test]
fn srcset_density_affects_object_fit_none() {
  let green_4x4 = solid_color_png(4, 4, [0, 255, 0, 255]);
  let html = format!(
    r#"
    <style>
      body {{ margin:0; background: rgb(255 0 0); }}
      img {{
        display:block;
        width:4px;
        height:4px;
        object-fit:none;
        object-position:left top;
      }}
    </style>
    <img srcset="{green_4x4} 2x">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 4, 4).expect("render html");

  // With `srcset` density=2, the chosen 4×4 image has a natural size of 2×2 CSS px, so
  // `object-fit:none` paints only the top-left 2×2 of the 4×4 replaced box and leaves the
  // bottom-right showing the red page background.
  let px = pixmap.pixel(3, 3).expect("pixel (3,3)");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected bottom-right pixel to show red background"
  );
}

#[test]
fn img_srcset_density_scales_natural_size_for_object_fit_none() {
  let src = solid_color_png(2, 2, [255, 0, 255, 255]);
  let srcset_2x = solid_color_png(4, 4, [0, 255, 0, 255]);
  let scale = 2.0;
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: rgb(255 0 0); }}
      img {{
        display: block;
        width: 6px;
        height: 6px;
        background: rgb(0 0 255);
        object-fit: none;
        object-position: 0 0;
      }}
    </style>
    <img src="{src}" srcset="{srcset_2x} 2x">
    "#
  );

  let legacy = render_with_backend(&html, 10, 10, scale, PaintBackend::Legacy);
  let display = render_with_backend(&html, 10, 10, scale, PaintBackend::DisplayList);

  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    let img_px = pixmap.pixel(2, 2).expect("pixel in bounds");
    assert_eq!(
      (img_px.red(), img_px.green(), img_px.blue()),
      (0, 255, 0),
      "{label}: expected image pixels"
    );

    let bg_px = pixmap.pixel(6, 2).expect("pixel in bounds");
    assert_eq!(
      (bg_px.red(), bg_px.green(), bg_px.blue()),
      (0, 0, 255),
      "{label}: expected element background where object-fit:none does not cover the box"
    );

    let outside_px = pixmap.pixel(14, 2).expect("pixel in bounds");
    assert_eq!(
      (outside_px.red(), outside_px.green(), outside_px.blue()),
      (255, 0, 0),
      "{label}: expected body/background pixels outside the element"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "rendered output diverged between backends"
  );
}
