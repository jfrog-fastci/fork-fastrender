use base64::{engine::general_purpose, Engine as _};
use fastrender::{FastRender, RenderOptions, ResourceKind};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder};

fn solid_color_png_data_url(width: u32, height: u32, rgba: [u8; 4]) -> String {
  let mut buf = Vec::new();
  let mut pixels = vec![0u8; (width * height * 4) as usize];
  for chunk in pixels.chunks_exact_mut(4) {
    chunk.copy_from_slice(&rgba);
  }
  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ColorType::Rgba8.into())
    .expect("encode png");
  format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&buf))
}

fn rgba_at(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn csp_img_src_wildcard_blocks_data_url_images() {
  crate::common::with_large_stack(|| {
    let green_png = solid_color_png_data_url(20, 20, [0, 255, 0, 255]);
    let html = format!(
      r#"<!doctype html>
        <html>
          <head>
            <style>
              body {{ margin:0; background: rgb(255 0 0); }}
            </style>
            <meta http-equiv="Content-Security-Policy" content="img-src *">
          </head>
          <body>
            <img src="{green_png}" style="display:block;width:20px;height:20px">
          </body>
        </html>"#
    );

    let mut renderer = FastRender::new().expect("renderer");
    let result = renderer
      .render_html_with_diagnostics(&html, RenderOptions::new().with_viewport(30, 30))
      .expect("render should succeed");

    assert_eq!(
      rgba_at(&result.pixmap, 10, 10),
      (255, 0, 0, 255),
      "expected img-src * to block data: images, leaving the red background visible"
    );

    assert!(
      result.diagnostics.fetch_errors.iter().any(|e| {
        e.kind == ResourceKind::Image
          && e.url.starts_with("data:image/png")
          && e.message.contains("Content-Security-Policy")
          && e.message.contains("img-src")
      }),
      "expected CSP img-src violation to be recorded for blocked data: image, got diagnostics={:?}",
      result.diagnostics.fetch_errors
    );
  });
}

#[test]
fn csp_img_src_data_allows_data_url_images() {
  crate::common::with_large_stack(|| {
    let green_png = solid_color_png_data_url(20, 20, [0, 255, 0, 255]);
    let html = format!(
      r#"<!doctype html>
        <html>
          <head>
            <style>
              body {{ margin:0; background: rgb(255 0 0); }}
            </style>
            <meta http-equiv="Content-Security-Policy" content="img-src data:">
          </head>
          <body>
            <img src="{green_png}" style="display:block;width:20px;height:20px">
          </body>
        </html>"#
    );

    let mut renderer = FastRender::new().expect("renderer");
    let result = renderer
      .render_html_with_diagnostics(&html, RenderOptions::new().with_viewport(30, 30))
      .expect("render should succeed");

    assert_eq!(
      rgba_at(&result.pixmap, 10, 10),
      (0, 255, 0, 255),
      "expected img-src data: to allow data: images to render"
    );

    assert!(
      !result.diagnostics.fetch_errors.iter().any(|e| {
        e.kind == ResourceKind::Image
          && e.url.starts_with("data:image/png")
          && e.message.contains("Content-Security-Policy")
          && e.message.contains("img-src")
      }),
      "expected no CSP img-src violation for permitted data: image, got diagnostics={:?}",
      result.diagnostics.fetch_errors
    );
  });
}

