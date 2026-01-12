use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use crate::geometry::Point;
use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::{FastRender, Pixmap};
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

fn solid_color_png(r: u8, g: u8, b: u8, a: u8) -> String {
  let mut buf = Vec::new();
  PngEncoder::new(&mut buf)
    .write_image(&[r, g, b, a], 1, 1, ExtendedColorType::Rgba8)
    .expect("encode png");
  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

fn render_with_backend(html: &str, width: u32, height: u32, backend: PaintBackend) -> Pixmap {
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
    1.0,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    backend,
  )
  .expect("paint")
}

#[test]
fn img_srcset_decode_failure_does_not_fallback_to_src() {
  let src = solid_color_png(0, 255, 0, 255);
  let broken = "data:text/plain,not-an-image";
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: rgb(255 0 0); }}
      img {{ display: block; width: 20px; height: 20px; }}
    </style>
    <img src="{src}" srcset="{broken} 1x">
    "#
  );

  let legacy = render_with_backend(&html, 30, 30, PaintBackend::Legacy);
  let display = render_with_backend(&html, 30, 30, PaintBackend::DisplayList);

  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    let px = pixmap.pixel(10, 10).expect("inside pixel");
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (255, 0, 0),
      "{label}: should not fall back to the `src` image when the selected `srcset` candidate fails to decode"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "rendered output diverged between backends"
  );
}
