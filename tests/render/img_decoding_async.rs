use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig, Result};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MapFetcher {
  resources: HashMap<String, Vec<u8>>,
  mime: String,
  fetch_count: AtomicUsize,
}

impl ResourceFetcher for MapFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.fetch_count.fetch_add(1, Ordering::Relaxed);
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
  paint_backend: &str,
) -> Result<tiny_skia::Pixmap> {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    paint_backend.to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_default_viewport(width, height)
    .with_runtime_toggles(toggles);
  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher)).expect("renderer");
  renderer.render_html(html, width, height)
}

#[test]
fn img_decoding_async_defers_only_for_large_destinations() {
  let big_png = png_with_dimensions_and_color(2000, 1333, [255, 0, 0, 255]);
  let resources = HashMap::from([("test://big.png".to_string(), big_png)]);
  let fetcher = Arc::new(MapFetcher {
    resources,
    mime: "image/png".to_string(),
    fetch_count: AtomicUsize::new(0),
  }) as Arc<dyn ResourceFetcher>;

  // Medium destination sizes should still paint even when `decoding="async"` is set (Chrome often
  // decodes these quickly enough for headless screenshot baselines).
  let html_async_small = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 448px; height: 299px; }
      </style>
      <img decoding="async" src="test://big.png">
    "#;
  let pixmap_async_small =
    render_single_img(html_async_small, Arc::clone(&fetcher), 448, 299, "display_list")
      .expect("render async small");
  let px = pixmap_async_small.pixel(224, 149).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected decoding=async image to paint at moderate destination sizes"
  );

  // Keep the destination paint size large enough that `decoding="async"` may still be pending in a
  // headless Chrome baseline screenshot (similar to USA Today fixtures).
  let html_async = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 512px; height: 512px; }
      </style>
      <img decoding="async" src="test://big.png">
    "#;
  let pixmap_async =
    render_single_img(html_async, Arc::clone(&fetcher), 512, 512, "display_list")
      .expect("render async");
  let px = pixmap_async.pixel(256, 256).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (0, 255, 0),
    "expected deferred async decode to keep img transparent (show background)"
  );

  // `loading="eager"` should override the async decode deferral heuristics so that above-the-fold
  // hero images still paint even when `decoding="async"` is set.
  let html_async_eager = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 512px; height: 512px; }
      </style>
      <img loading="eager" decoding="async" src="test://big.png">
    "#;
  let pixmap_async_eager =
    render_single_img(html_async_eager, Arc::clone(&fetcher), 512, 512, "display_list")
      .expect("render async eager");
  let px = pixmap_async_eager.pixel(256, 256).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected decoding=async + loading=eager image to paint"
  );

  let html_sync = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 255, 0); }
        img { display: block; width: 512px; height: 512px; }
      </style>
      <img decoding="sync" src="test://big.png">
    "#;
  let pixmap_sync =
    render_single_img(html_sync, fetcher, 512, 512, "display_list").expect("render sync");
  let px = pixmap_sync.pixel(256, 256).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected decoding=sync image to paint"
  );
}

#[test]
fn img_loading_lazy_defers_only_when_offscreen() {
  for paint_backend in ["display_list", "legacy"] {
    let png = png_with_dimensions_and_color(100, 100, [255, 0, 0, 255]);
    let resources = HashMap::from([("test://img.png".to_string(), png)]);
    let fetcher = Arc::new(MapFetcher {
      resources,
      mime: "image/png".to_string(),
      fetch_count: AtomicUsize::new(0),
    });

    let html_lazy_visible = r#"
        <!doctype html>
        <style>
          html, body { margin: 0; background: rgb(0, 255, 0); }
          img { display: block; width: 100px; height: 100px; }
        </style>
        <img loading="lazy" src="test://img.png">
      "#;
    let pixmap_lazy_visible =
      render_single_img(html_lazy_visible, fetcher.clone(), 100, 100, paint_backend)
        .unwrap_or_else(|_| panic!("render lazy visible (backend={paint_backend})"));
    let px = pixmap_lazy_visible.pixel(50, 50).expect("pixel");
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (255, 0, 0),
      "expected loading=lazy image to paint when it intersects the viewport (backend={paint_backend})"
    );
    assert_eq!(
      fetcher.fetch_count.load(Ordering::Relaxed),
      1,
      "expected visible loading=lazy image to fetch and decode (backend={paint_backend})"
    );

    let png = png_with_dimensions_and_color(100, 100, [255, 0, 0, 255]);
    let resources = HashMap::from([("test://img.png".to_string(), png)]);
    let fetcher = Arc::new(MapFetcher {
      resources,
      mime: "image/png".to_string(),
      fetch_count: AtomicUsize::new(0),
    });

    let html_lazy_offscreen = r#"
        <!doctype html>
        <style>
          html, body { margin: 0; background: rgb(0, 255, 0); }
          .spacer { height: 200px; }
          img { display: block; width: 100px; height: 100px; }
        </style>
        <div class="spacer"></div>
        <img loading="lazy" src="test://img.png">
      "#;
    let pixmap_lazy_offscreen =
      render_single_img(html_lazy_offscreen, fetcher.clone(), 100, 100, paint_backend)
        .unwrap_or_else(|_| panic!("render lazy offscreen (backend={paint_backend})"));
    let px = pixmap_lazy_offscreen.pixel(50, 50).expect("pixel");
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (0, 255, 0),
      "expected offscreen loading=lazy image to not paint into the viewport (backend={paint_backend})"
    );
    assert_eq!(
      fetcher.fetch_count.load(Ordering::Relaxed),
      0,
      "expected offscreen loading=lazy image to not fetch or decode (backend={paint_backend})"
    );

    let html_eager = r#"
        <!doctype html>
        <style>
          html, body { margin: 0; background: rgb(0, 255, 0); }
          img { display: block; width: 100px; height: 100px; }
        </style>
        <img loading="eager" src="test://img.png">
      "#;
    let pixmap_eager = render_single_img(html_eager, fetcher.clone(), 100, 100, paint_backend)
      .unwrap_or_else(|_| panic!("render eager (backend={paint_backend})"));
    let px = pixmap_eager.pixel(50, 50).expect("pixel");
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (255, 0, 0),
      "expected loading=eager image to paint (backend={paint_backend})"
    );
  }
}
