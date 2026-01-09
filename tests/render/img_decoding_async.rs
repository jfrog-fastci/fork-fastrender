use fastrender::{FastRender, Result};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::collections::HashMap;
use std::sync::Arc;

struct MapFetcher {
  resources: HashMap<String, Vec<u8>>,
  mime: String,
}

impl ResourceFetcher for MapFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let bytes = self
      .resources
      .get(url)
      .unwrap_or_else(|| panic!("unexpected url fetch: {url}"))
      .clone();
    Ok(FetchedResource::new(bytes, Some(self.mime.clone())))
  }
}

fn png_with_dimensions_and_color(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
  let image = RgbaImage::from_pixel(width, height, Rgba(color));
  let mut cursor = std::io::Cursor::new(Vec::new());
  DynamicImage::ImageRgba8(image)
    .write_to(&mut cursor, ImageFormat::Png)
    .expect("encode png");
  cursor.into_inner()
}

fn render_single_img(
  html: &str,
  fetcher: Arc<dyn ResourceFetcher>,
  width: u32,
  height: u32,
) -> Result<tiny_skia::Pixmap> {
  let mut renderer = FastRender::builder()
    .viewport_size(width, height)
    .fetcher(fetcher)
    .build()
    .expect("renderer");
  renderer.render_html(html, width, height)
}

#[test]
fn large_img_decoding_async_is_transparent_while_sync_paints() {
  let big_png = png_with_dimensions_and_color(2000, 1333, [255, 0, 0, 255]);
  let resources = HashMap::from([("test://big.png".to_string(), big_png)]);
  let fetcher = Arc::new(MapFetcher {
    resources,
    mime: "image/png".to_string(),
  }) as Arc<dyn ResourceFetcher>;

  // Keep the destination paint size large enough that `decoding="async"` may still be pending in a
  // headless Chrome baseline screenshot (similar to USA Today fixtures).
  let html_async = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 384px; height: 384px; }
      </style>
      <img decoding="async" src="test://big.png">
    "#;
  let pixmap_async =
    render_single_img(html_async, Arc::clone(&fetcher), 384, 384).expect("render async");
  let px = pixmap_async.pixel(192, 192).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (0, 255, 0),
    "expected deferred async decode to keep img transparent (show background)"
  );

  let html_sync = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 384px; height: 384px; }
      </style>
      <img decoding="sync" src="test://big.png">
    "#;
  let pixmap_sync = render_single_img(html_sync, fetcher, 384, 384).expect("render sync");
  let px = pixmap_sync.pixel(192, 192).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected decoding=sync image to paint"
  );
}
