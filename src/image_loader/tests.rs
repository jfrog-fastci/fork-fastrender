use crate::api::ResourceContext;
use crate::error::{Error, Result};
use crate::image_loader::ImageCache;
use crate::resource::{
  origin_from_url, FetchDestination, FetchRequest, FetchedResource, ReferrerPolicy, ResourceFetcher,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::{ImageFormat, Rgba, RgbaImage};
use resvg::usvg;
use std::fs;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};
use url::Url;

#[test]
fn usvg_options_for_url_does_not_enable_filesystem_resource_dir() {
  let dir = tempfile::tempdir().expect("temp dir");
  let svg_path = dir.path().join("icon.svg");
  let svg_url = Url::from_file_path(&svg_path).unwrap().to_string();

  let options = super::usvg_options_for_url(&svg_url);
  assert!(
    options.resources_dir.is_none(),
    "expected usvg options to keep resources_dir unset so usvg/resvg cannot read relative files from disk"
  );
}

#[derive(Default)]
struct RecordingFetcher {
  calls: Mutex<Vec<(String, Option<String>)>>,
}

impl RecordingFetcher {
  fn count_url(&self, url: &str) -> usize {
    self
      .calls
      .lock()
      .expect("lock")
      .iter()
      .filter(|(u, _)| u == url)
      .count()
  }
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self
      .calls
      .lock()
      .expect("lock")
      .push((req.url.to_string(), req.referrer_url.map(|r| r.to_string())));

    let referrer = req.referrer_url.unwrap_or_default();
    let client_origin = req.client_origin.map(|o| o.to_string()).unwrap_or_default();
    match req.url {
      "https://example.com/sprite.svg" => {
        let color = if referrer.ends_with("/a.svg") { "red" } else { "blue" };
        let svg = format!(
          r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="{color}"/></symbol></svg>"#
        );
        Ok(FetchedResource::new(svg.into_bytes(), Some("image/svg+xml".to_string())))
      }
      "https://example.com/sprite_by_origin.svg" => {
        let color = if client_origin.contains("a.example") { "red" } else { "blue" };
        let svg = format!(
          r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="{color}"/></symbol></svg>"#
        );
        Ok(FetchedResource::new(svg.into_bytes(), Some("image/svg+xml".to_string())))
      }
      "https://example.com/sprite_by_policy.svg" => {
        let color = if req.referrer_policy == ReferrerPolicy::NoReferrer {
          "red"
        } else {
          "blue"
        };
        let svg = format!(
          r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="{color}"/></symbol></svg>"#
        );
        Ok(FetchedResource::new(svg.into_bytes(), Some("image/svg+xml".to_string())))
      }
      "https://example.com/img.png" => {
        let bytes = if referrer.ends_with("/a.svg") { b"A".to_vec() } else { b"B".to_vec() };
        Ok(FetchedResource::new(bytes, None))
      }
      "https://example.com/img_by_origin.png" => {
        let bytes = if client_origin.contains("a.example") {
          b"C".to_vec()
        } else {
          b"D".to_vec()
        };
        Ok(FetchedResource::new(bytes, None))
      }
      "https://example.com/img_by_policy.png" => {
        let bytes = if req.referrer_policy == ReferrerPolicy::NoReferrer {
          b"P".to_vec()
        } else {
          b"Q".to_vec()
        };
        Ok(FetchedResource::new(bytes, None))
      }
      other => Err(Error::Other(format!("unexpected url {other}"))),
    }
  }
}

#[test]
fn image_cache_with_fetcher_loads_image_from_mock_fetcher() {
  use std::sync::atomic::{AtomicUsize, Ordering};

  struct PngFetcher {
    bytes: Vec<u8>,
    calls: AtomicUsize,
  }

  impl ResourceFetcher for PngFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      assert_eq!(url, "https://example.com/red.png");
      self.calls.fetch_add(1, Ordering::SeqCst);
      Ok(FetchedResource::new(
        self.bytes.clone(),
        Some("image/png".to_string()),
      ))
    }
  }

  let img = RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255]));
  let mut png = Vec::new();
  img
    .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
    .expect("encode png");

  let fetcher = Arc::new(PngFetcher {
    bytes: png,
    calls: AtomicUsize::new(0),
  });
  let cache = ImageCache::with_fetcher(Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>);

  let loaded = cache.load("https://example.com/red.png").expect("load image");
  let rgba = loaded.image.to_rgba8();
  assert_eq!(rgba.dimensions(), (2, 2));
  assert_eq!(*rgba.get_pixel(0, 0), Rgba([255, 0, 0, 255]));

  // Second load should hit the decoded cache (no additional fetch).
  let _ = cache.load("https://example.com/red.png").expect("load again");
  assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
}

#[cfg(not(feature = "direct_network"))]
#[test]
fn image_cache_new_requires_injected_fetcher_in_sandboxed_builds() {
  let cache = ImageCache::new();
  let err = cache
    .fetcher()
    .fetch("https://example.com/image.png")
    .expect_err("expected sandboxed ImageCache to reject network fetches");
  match err {
    Error::Resource(res) => {
      assert_eq!(
        res.message,
        "ImageCache requires an injected ResourceFetcher in sandboxed builds"
      );
    }
    other => panic!("expected Error::Resource, got {other:?}"),
  }
}

#[test]
fn svg_root_viewport_resolves_percent_lengths_for_rasterization() {
  // Regression test for SVG-as-image rasterization: when the outermost <svg> omits width/height,
  // SVG defaults them to 100%. Percent-based sizes (like <image width="100%">) must then resolve
  // against the concrete render size supplied by the embedding context.
  //
  // `resvg/usvg` resolve percentage lengths during parse. If the outermost viewport is missing,
  // percent values can collapse to zero, producing fully transparent output. Ensure we inject a
  // definite viewport size before rasterization.
  let cache = ImageCache::new();

  // This is a minimized version of Next.js' blur placeholder SVG (as seen on theverge.com):
  // - outermost <svg> has no width/height (defaults to 100%),
  // - <image width="100%" height="100%"> is filtered via `style="filter: url(#b);"`
  // - referenced PNG is a 1×1 opaque light pixel (L=233, A=255).
  //
  // If percent lengths in SVG are resolved without the concrete raster size, the <image> can end
  // up with a 0×0 bounding box and the entire render becomes transparent.
  const PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mN8+R8AAtcB6oaHtZcAAAAASUVORK5CYII=";
  let svg = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' ><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let svg_explicit = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let pixmap_explicit = cache
    .render_svg_pixmap_at_size(&svg_explicit, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap (explicit)");
  let explicit_center = pixmap_explicit.pixel(50, 50).expect("center pixel");
  assert!(
    explicit_center.alpha() > 0,
    "setup sanity check: expected explicit-viewport SVG to render; got rgba=({}, {}, {}, {})",
    explicit_center.red(),
    explicit_center.green(),
    explicit_center.blue(),
    explicit_center.alpha()
  );

  let pixmap = cache
    .render_svg_pixmap_at_size(&svg, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap");

  let center = pixmap.pixel(50, 50).expect("center pixel");
  assert!(
    center.alpha() > 0,
    "expected filtered percent-sized <image> to render when root viewport is implicit; got rgba=({}, {}, {}, {})",
    center.red(),
    center.green(),
    center.blue(),
    center.alpha()
  );
}

#[test]
fn resvg_ignores_css_transform_translate_percent() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="30" height="10" viewBox="0 0 30 10" shape-rendering="crispEdges">
      <g style="transform-box: fill-box; transform: translateX(100%);">
        <rect width="10" height="10" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("parse svg");
  let mut pixmap = Pixmap::new(30, 10).expect("pixmap");
  resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());

  let red = PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("color");
  let pixels = pixmap.pixels_mut();
  let at = |x: u32, y: u32| pixels[(y * 30 + x) as usize];

  assert_eq!(
    at(5, 5),
    red,
    "resvg/usvg currently ignores CSS `transform` in the style attribute (percent case)"
  );
  assert_eq!(
    at(15, 5),
    PremultipliedColorU8::TRANSPARENT,
    "rect should remain at the origin when CSS `transform` is ignored"
  );
}

#[cfg(feature = "direct_network")]
#[test]
fn svg_image_href_resolves_against_svg_url() {
  let dir = tempfile::tempdir().expect("temp dir");
  let png_path = dir.path().join("img.png");
  let png = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
  png.save(&png_path).expect("write png");

  let svg_path = dir.path().join("icon.svg");
  let svg_content = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
      <image href="img.png" width="4" height="4" />
    </svg>
  "#;
  fs::write(&svg_path, svg_content).expect("write svg");

  let svg_url = url::Url::from_file_path(&svg_path).unwrap().to_string();

  let mut cache = ImageCache::new();
  cache.set_base_url("file:///not-used-for-svg-base/");

  let image = cache.load(&svg_url).expect("render svg with image href");
  let rgba = image.image.to_rgba8();

  assert_eq!(rgba.dimensions(), (4, 4));
  assert_eq!(*rgba.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
  assert_eq!(*rgba.get_pixel(3, 3), Rgba([255, 0, 0, 255]));
}

#[test]
fn svg_image_href_supports_data_url() {
  let mut cache = ImageCache::new();

  let data_image = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 255, 255]));
  let mut buf = Vec::new();
  data_image
    .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
    .expect("encode png");
  let data_url = format!("data:image/png;base64,{}", STANDARD.encode(&buf));

  let svg = format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
        <image href="{data_url}" width="2" height="2" />
      </svg>"#
  );

  let (rendered, _, _) = cache.render_svg_to_image(&svg).expect("render svg");
  let rgba = rendered.to_rgba8();
  assert_eq!(*rgba.get_pixel(0, 0), Rgba([0, 0, 255, 255]));
  assert_eq!(*rgba.get_pixel(1, 1), Rgba([0, 0, 255, 255]));
}

fn composite_pixel_over_white(px: Rgba<u8>) -> Rgba<u8> {
  let a = px[3] as u16;
  let inv_a = 255u16.saturating_sub(a);
  // `render_svg_to_image` rasterizes via `tiny-skia`, which stores pixels as premultiplied RGBA.
  // We only sample fully-opaque / fully-transparent pixels in these tests, but compositing over a
  // white background makes the expected colors easier to express (red vs white).
  Rgba([
    (px[0] as u16 + inv_a).min(255) as u8,
    (px[1] as u16 + inv_a).min(255) as u8,
    (px[2] as u16 + inv_a).min(255) as u8,
    255,
  ])
}

fn render_svg(svg: &str) -> RgbaImage {
  let cache = ImageCache::new();
  let (img, _ratio, _aspect_ratio_none) = cache.render_svg_to_image(svg).expect("render svg");
  img.to_rgba8()
}

#[test]
fn svg_render_to_image_preserve_aspect_ratio_xmin_ymin_meet() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg"
         width="20" height="10"
         viewBox="0 0 10 10"
         preserveAspectRatio="xMinYMin meet"
         shape-rendering="crispEdges">
      <rect x="0" y="0" width="10" height="10" fill="red" />
    </svg>
  "#;

  let rgba = render_svg(svg);
  assert_eq!(rgba.dimensions(), (20, 10));

  // With `meet`, the 10x10 viewBox fits into a 20x10 viewport without scaling (height is the
  // limiting dimension), leaving 10px of horizontal space. `xMinYMin` aligns the content to the
  // left.
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(2, 5)),
    Rgba([255, 0, 0, 255]),
    "expected left side to be red"
  );
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(18, 5)),
    Rgba([255, 255, 255, 255]),
    "expected right side to be empty (white when composited)"
  );
}

#[test]
fn svg_render_to_image_preserve_aspect_ratio_xmax_ymin_meet() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg"
         width="20" height="10"
         viewBox="0 0 10 10"
         preserveAspectRatio="xMaxYMin meet"
         shape-rendering="crispEdges">
      <rect x="0" y="0" width="10" height="10" fill="red" />
    </svg>
  "#;

  let rgba = render_svg(svg);
  assert_eq!(rgba.dimensions(), (20, 10));

  // `xMaxYMin` aligns the viewBox content to the right.
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(2, 5)),
    Rgba([255, 255, 255, 255]),
    "expected left side to be empty (white when composited)"
  );
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(18, 5)),
    Rgba([255, 0, 0, 255]),
    "expected right side to be red"
  );
}

#[test]
fn svg_subresource_cache_partitions_by_referrer_for_sprites() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let ctx = ResourceContext::default();

  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><use href="https://example.com/sprite.svg#icon"/></svg>"#;

  let out_a_1 = super::inline_svg_use_references(
    svg,
    "https://example.com/a.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <use> (a)")
  .into_owned();
  let out_b = super::inline_svg_use_references(
    svg,
    "https://example.com/b.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <use> (b)")
  .into_owned();
  let out_a_2 = super::inline_svg_use_references(
    svg,
    "https://example.com/a.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <use> (a again)")
  .into_owned();

  assert!(
    out_a_1.contains(r#"fill="red""#),
    "expected sprite variant for ctx_a, got: {out_a_1}"
  );
  assert!(
    out_b.contains(r#"fill="blue""#),
    "expected sprite variant for ctx_b, got: {out_b}"
  );
  assert_eq!(out_a_1, out_a_2, "expected ctx_a to hit svg_subresource_cache");
  assert_eq!(
    fetcher.count_url("https://example.com/sprite.svg"),
    2,
    "expected sprite to be fetched once per referrer context"
  );
}

#[test]
fn svg_subresource_cache_partitions_by_referrer_for_inlined_images() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let ctx = ResourceContext::default();

  let svg =
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://example.com/img.png" width="1" height="1"/></svg>"#;

  let out_a_1 = super::inline_svg_image_references(
    svg,
    "https://example.com/a.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <image> (a)")
  .into_owned();
  let out_b = super::inline_svg_image_references(
    svg,
    "https://example.com/b.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <image> (b)")
  .into_owned();
  let out_a_2 = super::inline_svg_image_references(
    svg,
    "https://example.com/a.svg",
    &fetcher,
    Some(&ctx),
    Some(&subresource_cache),
  )
  .expect("inline <image> (a again)")
  .into_owned();

  assert!(
    out_a_1.contains("QQ=="),
    "expected data URL for ctx_a (base64 A=QQ==), got: {out_a_1}"
  );
  assert!(
    out_b.contains("Qg=="),
    "expected data URL for ctx_b (base64 B=Qg==), got: {out_b}"
  );
  assert_eq!(out_a_1, out_a_2, "expected ctx_a to hit svg_subresource_cache");
  assert_eq!(
    fetcher.count_url("https://example.com/img.png"),
    2,
    "expected image to be fetched once per referrer context"
  );
}

#[test]
fn svg_subresource_cache_partitions_by_client_origin_for_sprites() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let mut ctx_a = ResourceContext::default();
  ctx_a.policy.document_origin = origin_from_url("https://a.example");
  assert!(ctx_a.policy.document_origin.is_some(), "origin a");
  let mut ctx_b = ResourceContext::default();
  ctx_b.policy.document_origin = origin_from_url("https://b.example");
  assert!(ctx_b.policy.document_origin.is_some(), "origin b");

  let svg =
    r#"<svg xmlns="http://www.w3.org/2000/svg"><use href="https://example.com/sprite_by_origin.svg#icon"/></svg>"#;

  let out_a_1 = super::inline_svg_use_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <use> (origin a)")
  .into_owned();
  let out_b = super::inline_svg_use_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_b),
    Some(&subresource_cache),
  )
  .expect("inline <use> (origin b)")
  .into_owned();
  let out_a_2 = super::inline_svg_use_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <use> (origin a again)")
  .into_owned();

  assert!(
    out_a_1.contains(r#"fill="red""#),
    "expected sprite variant for origin a, got: {out_a_1}"
  );
  assert!(
    out_b.contains(r#"fill="blue""#),
    "expected sprite variant for origin b, got: {out_b}"
  );
  assert_eq!(out_a_1, out_a_2, "expected origin a to hit svg_subresource_cache");
  assert_eq!(
    fetcher.count_url("https://example.com/sprite_by_origin.svg"),
    2,
    "expected sprite to be fetched once per client origin"
  );
}

#[test]
fn svg_subresource_cache_partitions_by_client_origin_for_inlined_images() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let mut ctx_a = ResourceContext::default();
  ctx_a.policy.document_origin = origin_from_url("https://a.example");
  assert!(ctx_a.policy.document_origin.is_some(), "origin a");
  let mut ctx_b = ResourceContext::default();
  ctx_b.policy.document_origin = origin_from_url("https://b.example");
  assert!(ctx_b.policy.document_origin.is_some(), "origin b");

  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://example.com/img_by_origin.png" width="1" height="1"/></svg>"#;

  let out_a_1 = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <image> (origin a)")
  .into_owned();
  let out_b = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_b),
    Some(&subresource_cache),
  )
  .expect("inline <image> (origin b)")
  .into_owned();
  let out_a_2 = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <image> (origin a again)")
  .into_owned();

  assert!(
    out_a_1.contains("Qw=="),
    "expected data URL for origin a (base64 C=Qw==), got: {out_a_1}"
  );
  assert!(
    out_b.contains("RA=="),
    "expected data URL for origin b (base64 D=RA==), got: {out_b}"
  );
  assert_eq!(out_a_1, out_a_2, "expected origin a to hit svg_subresource_cache");
  assert_eq!(
    fetcher.count_url("https://example.com/img_by_origin.png"),
    2,
    "expected image to be fetched once per client origin"
  );
}

#[test]
fn svg_subresource_cache_partitions_by_referrer_policy_for_inlined_images() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let mut ctx_a = ResourceContext::default();
  ctx_a.referrer_policy = ReferrerPolicy::NoReferrer;
  let mut ctx_b = ResourceContext::default();
  ctx_b.referrer_policy = ReferrerPolicy::Origin;

  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://example.com/img_by_policy.png" width="1" height="1"/></svg>"#;

  let out_a_1 = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <image> (policy a)")
  .into_owned();
  let out_b = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_b),
    Some(&subresource_cache),
  )
  .expect("inline <image> (policy b)")
  .into_owned();
  let out_a_2 = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_a),
    Some(&subresource_cache),
  )
  .expect("inline <image> (policy a again)")
  .into_owned();

  assert!(
    out_a_1.contains("UA=="),
    "expected data URL for NoReferrer (base64 P=UA==), got: {out_a_1}"
  );
  assert!(
    out_b.contains("UQ=="),
    "expected data URL for Origin (base64 Q=UQ==), got: {out_b}"
  );
  assert_eq!(out_a_1, out_a_2, "expected NoReferrer ctx to hit svg_subresource_cache");
  assert_eq!(
    fetcher.count_url("https://example.com/img_by_policy.png"),
    2,
    "expected image to be fetched once per referrer policy"
  );
}

#[test]
fn svg_subresource_cache_referrer_policy_empty_string_matches_chromium_default() {
  let fetcher = RecordingFetcher::default();
  let subresource_cache: super::SvgSubresourceCache =
    Arc::new(Mutex::new(super::SizedLruCache::new(64, 1024 * 1024)));

  let ctx_empty = ResourceContext::default();
  let mut ctx_default = ResourceContext::default();
  ctx_default.referrer_policy = ReferrerPolicy::CHROMIUM_DEFAULT;

  let svg =
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://example.com/img.png" width="1" height="1"/></svg>"#;

  let out_empty = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_empty),
    Some(&subresource_cache),
  )
  .expect("inline <image> (empty policy)")
  .into_owned();
  let out_default = super::inline_svg_image_references(
    svg,
    "https://example.com/importer.svg",
    &fetcher,
    Some(&ctx_default),
    Some(&subresource_cache),
  )
  .expect("inline <image> (chromium default policy)")
  .into_owned();

  assert_eq!(
    out_empty, out_default,
    "expected empty-string and chromium default referrer policy to share svg_subresource_cache"
  );
  assert_eq!(
    fetcher.count_url("https://example.com/img.png"),
    1,
    "expected fetch to occur once when policy is effectively the Chromium default"
  );
}
