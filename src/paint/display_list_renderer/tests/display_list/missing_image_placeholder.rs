#![cfg(test)]

use crate::debug::runtime::RuntimeToggles;
use crate::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use crate::{FastRender, FastRenderConfig};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder, RgbaImage};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::tempdir;
use tiny_skia::Pixmap;
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

fn count_red(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
        total += 1;
      }
    }
  }
  total
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
  fn fetch(&self, url: &str) -> crate::Result<FetchedResource> {
    self
      .entries
      .get(url)
      .cloned()
      .ok_or_else(|| crate::Error::Other(format!("unexpected fetch: {url}")))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> crate::Result<FetchedResource> {
    // Ensure tests fail loudly if the request is not for an image.
    assert!(
      matches!(
        req.destination,
        FetchDestination::Image | FetchDestination::ImageCors
      ),
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

  // The broken-image placeholder keeps the image box transparent (so author-provided backgrounds
  // show through); the only non-transparent pixels should come from the UA icon.
  let bg_px = pixmap.pixel(0, 0).expect("pixel in bounds"); // fastrender-allow-unwrap
  assert_eq!(
    (bg_px.red(), bg_px.green(), bg_px.blue(), bg_px.alpha()),
    (0, 0, 0, 255),
    "expected background to show through at top-left (no full-frame border)",
  );

  // The UA icon has a crisp 1px border.
  let icon_border_px =
    pixmap.pixel(2, 2).expect("icon border pixel in bounds"); // fastrender-allow-unwrap
  assert_eq!(
    (
      icon_border_px.red(),
      icon_border_px.green(),
      icon_border_px.blue(),
      icon_border_px.alpha()
    ),
    (163, 163, 163, 255),
    "expected broken-image icon border to be gray, got {:?}",
    (
      icon_border_px.red(),
      icon_border_px.green(),
      icon_border_px.blue(),
      icon_border_px.alpha()
    )
  );

  // Chrome's broken-image placeholder keeps the image box transparent (so author-provided
  // backgrounds show through) but draws a small icon in the top-left.
  let icon_px = pixmap.pixel(4, 4).expect("icon pixel in bounds");
  assert!(
    icon_px.red() > 100 && icon_px.green() > 100 && icon_px.blue() > 100 && icon_px.alpha() == 255,
    "expected broken-image icon to paint a light pixel, got {:?}",
    (
      icon_px.red(),
      icon_px.green(),
      icon_px.blue(),
      icon_px.alpha()
    )
  );

  let interior_px = pixmap.pixel(15, 15).expect("interior pixel in bounds");
  assert_eq!(
    (
      interior_px.red(),
      interior_px.green(),
      interior_px.blue(),
      interior_px.alpha()
    ),
    (0, 0, 0, 255),
    "expected placeholder interior to remain transparent over black background",
  );
}

#[test]
fn display_list_img_alt_text_wraps_within_replaced_box() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let tmp = tempdir().expect("tempdir");
  let empty_path = tmp.path().join("empty.bin");
  std::fs::write(&empty_path, []).expect("write empty image");
  let empty_url = Url::from_file_path(&empty_path).expect("file URL");

  let html = format!(
    "<!doctype html>\
     <style>\
       html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
       img {{ display: block; width: 60px; height: 60px; font-size: 16px; line-height: 16px; color: rgb(255, 0, 0); }}\
     </style>\
     <img src=\"{empty_url}\" alt=\"a a a a a a a a a a a a a a a a a a a a a a a a\">"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 60, 60).expect("render");

  // If the alt text is wrapped into multiple lines, we should see red glyph pixels both near the
  // top of the replaced box and further down, while the top-left background remains transparent.
  let bg_px = pixmap.pixel(0, 0).expect("pixel in bounds"); // fastrender-allow-unwrap
  assert_eq!(
    (bg_px.red(), bg_px.green(), bg_px.blue(), bg_px.alpha()),
    (0, 0, 0, 255),
    "expected background to show through at top-left (no full-frame border)",
  );
  let icon_border_px =
    pixmap.pixel(2, 2).expect("icon border pixel in bounds"); // fastrender-allow-unwrap
  assert_eq!(
    (
      icon_border_px.red(),
      icon_border_px.green(),
      icon_border_px.blue(),
      icon_border_px.alpha()
    ),
    (163, 163, 163, 255),
    "expected broken-image icon border to be gray"
  );

  let top_red = count_red(&pixmap, 0, 0, 60, 20);
  let bottom_red = count_red(&pixmap, 0, 30, 60, 60);
  assert!(
    top_red > 0,
    "expected red alt text pixels near top (got {top_red})"
  );
  assert!(
    bottom_red > 0,
    "expected wrapped alt text to reach bottom half (got {bottom_red})"
  );
}

#[test]
fn display_list_img_alt_text_honors_text_align_when_painted_with_broken_icon() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let tmp = tempdir().expect("tempdir");
  let empty_path = tmp.path().join("empty.bin");
  std::fs::write(&empty_path, []).expect("write empty image");
  let empty_url = Url::from_file_path(&empty_path).expect("file URL");

  // Chrome keeps the broken-image icon pinned to the start edge of the replaced box, but the
  // accompanying alt text is laid out in the remaining space and can be affected by `text-align`.
  //
  // This is important on real pages that center-align hero content (e.g. kotlinlang.org), where
  // forcing start alignment causes the alt text to appear in the wrong place relative to Chrome.
  let html = format!(
    "<!doctype html>\
     <style>\
       html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
       img {{ display: block; width: 200px; height: 40px; font-size: 16px; line-height: 16px; color: rgb(255, 0, 0); text-align: center; }}\
     </style>\
     <img src=\"{empty_url}\" alt=\"X\">"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 200, 40).expect("render");

  // The broken-image icon consumes the first ~20px (inset + 16px icon + gap). With `text-align:
  // center`, the 'X' should land much further to the right than start-aligned text.
  let left_red = count_red(&pixmap, 20, 0, 60, 40);
  let center_red = count_red(&pixmap, 90, 0, 130, 40);
  assert!(center_red > 0, "expected centered red alt text pixels");
  assert!(
    left_red * 5 < center_red,
    "expected centered alt text (left_red={left_red}, center_red={center_red})"
  );
}

#[test]
fn display_list_img_alt_text_inherits_link_color_when_keyframes_present_but_no_animations() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let tmp = tempdir().expect("tempdir");
  let empty_path = tmp.path().join("empty.bin");
  std::fs::write(&empty_path, []).expect("write empty image");
  let empty_url = Url::from_file_path(&empty_path).expect("file URL");

  // Include a dummy @keyframes rule so `FragmentTree::keyframes` is non-empty, matching real pages
  // where keyframes exist even when animations are effectively disabled (the fixture harness often
  // injects `animation: none !important`). FastRender must treat this as a no-op and must not
  // recompute inherited colors based on the fragment tree (floats are reparented during layout).
  let html = format!(
    "<!doctype html>\
     <style>\
       @keyframes dummy {{ from {{ opacity: 1; }} to {{ opacity: 1; }} }}\
       html, body {{ margin: 0; background: rgb(0, 0, 0); color: rgb(0, 0, 0); }}\
       a {{ color: rgb(255, 0, 0); font: 20px/20px sans-serif; }}\
       .float {{ float: left; }}\
       img {{ display: block; width: 40px; height: 40px; }}\
     </style>\
     <a href=\"#\"><div class=\"float\"><img src=\"{empty_url}\" alt=\"X\"></div></a>"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 40, 40).expect("render");

  // Alt text starts to the right of the UA icon (x≈20px for a 40px image box).
  let red = count_red(&pixmap, 20, 0, 40, 40);
  assert!(red > 0, "expected red link-colored alt text pixels, got {red}");
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
    crate::resource::offline_placeholder_png_bytes(),
    "test requires non-canonical placeholder bytes"
  );

  let mut res = FetchedResource::new(
    bytes,
    Some(crate::resource::offline_placeholder_png_content_type().to_string()),
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
  assert_eq!( // fastrender-allow-panic
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
    crate::resource::offline_placeholder_png_bytes(),
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
  assert_eq!( // fastrender-allow-panic
    (px.red(), px.green(), px.blue(), px.alpha()),
    (0, 0, 0, 255)
  );
}
