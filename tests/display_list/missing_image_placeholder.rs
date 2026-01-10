use fastrender::debug::runtime::RuntimeToggles;
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::{FastRender, FastRenderConfig};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder, RgbaImage};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::tempdir;
use url::Url;

fn encode_single_pixel_png(rgba: [u8; 4]) -> Vec<u8> {
  let mut pixels = RgbaImage::new(1, 1);
  pixels.pixels_mut().for_each(|p| *p = image::Rgba(rgba));
  let mut png = Vec::new();
  PngEncoder::new(&mut png)
    .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
    .expect("encode png");
  png
}

#[derive(Clone)]
struct MapFetcher {
  entries: HashMap<String, FetchedResource>,
}

impl MapFetcher {
  fn new(entries: HashMap<String, FetchedResource>) -> Self {
    Self { entries }
  }
}

impl ResourceFetcher for MapFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    self
      .entries
      .get(url)
      .cloned()
      .ok_or_else(|| fastrender::Error::Other(format!("unexpected fetch: {url}")))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    // Ensure tests fail loudly if the request is not for an image.
    assert!(
      matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors),
      "unexpected fetch destination {:?} for {}",
      req.destination,
      req.url
    );
    self.fetch(req.url)
  }
}

#[test]
fn display_list_img_empty_bytes_renders_ua_placeholder() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // A 0-byte file should be treated as an invalid image. The image loader should resolve it to
  // the canonical transparent placeholder (so painters can detect and reject it), and then the
  // display-list builder should paint a UA "broken image" placeholder (matching Chrome).
  let tmp = tempdir().expect("tempdir");
  let empty_path = tmp.path().join("empty.bin");
  std::fs::write(&empty_path, []).expect("write empty image");
  let empty_url = Url::from_file_path(&empty_path).expect("file URL");

  let html = format!(
    "<!doctype html>\
    <style>\
      html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
      img {{ display: block; width: 20px; height: 20px; font-size: 20px; line-height: 1; color: transparent; }}\
    </style>\
    <img src=\"{empty_url}\" alt=\"X\">"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 24, 24).expect("render");

  // The placeholder fill should cover the image content box instead of leaving the black page
  // background visible. Sample a pixel well inside the 1px stroke.
  let inside = pixmap.pixel(2, 2).expect("pixel in bounds");
  assert!(
    inside.red() > 150 && inside.green() > 150 && inside.blue() > 150 && inside.alpha() == 255,
    "expected a light UA placeholder fill for missing image, got {:?}",
    (inside.red(), inside.green(), inside.blue(), inside.alpha())
  );
}

#[test]
fn display_list_img_marked_placeholder_png_renders_ua_broken_image_icon() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let url = "https://example.com/marked.png";
  let bytes = encode_single_pixel_png([0, 0, 0, 0]);
  assert_ne!(
    bytes.as_slice(),
    fastrender::resource::offline_placeholder_png_bytes(),
    "test requires non-canonical placeholder bytes"
  );

  let mut res = FetchedResource::new(
    bytes,
    Some(
      fastrender::resource::offline_placeholder_png_content_type()
        .to_string(),
    ),
  );
  res.status = Some(200);
  res.final_url = Some(url.to_string());

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(MapFetcher::new(HashMap::from([(url.to_string(), res)])));

  let html = format!(
    "<!doctype html>\
    <style>\
      html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
      img {{ display: block; width: 20px; height: 20px; }}\
    </style>\
    <img src=\"{url}\">"
  );

  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher)).expect("create renderer");
  let pixmap = renderer.render_html(&html, 24, 24).expect("render");

  // Sample a pixel inside the "sky" portion of the UA broken-image icon.
  let px = pixmap.pixel(4, 4).expect("pixel in bounds");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (198, 216, 244, 255)
  );
}

#[test]
fn display_list_img_unmarked_transparent_png_does_not_render_ua_broken_image_icon() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let url = "https://example.com/unmarked.png";
  let bytes = encode_single_pixel_png([0, 0, 0, 0]);
  assert_ne!(
    bytes.as_slice(),
    fastrender::resource::offline_placeholder_png_bytes(),
    "test requires non-canonical placeholder bytes"
  );

  let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
  res.status = Some(200);
  res.final_url = Some(url.to_string());

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(MapFetcher::new(HashMap::from([(url.to_string(), res)])));

  let html = format!(
    "<!doctype html>\
    <style>\
      html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
      img {{ display: block; width: 20px; height: 20px; }}\
    </style>\
    <img src=\"{url}\">"
  );

  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher)).expect("create renderer");
  let pixmap = renderer.render_html(&html, 24, 24).expect("render");

  // With a successfully decoded (but fully transparent) image, the black page background should
  // remain visible.
  let px = pixmap.pixel(4, 4).expect("pixel in bounds");
  assert_eq!((px.red(), px.green(), px.blue(), px.alpha()), (0, 0, 0, 255));
}
