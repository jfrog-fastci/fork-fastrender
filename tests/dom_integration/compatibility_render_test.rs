use fastrender::dom::DomCompatibilityMode;
use fastrender::{FastRender, RenderOptions, ResourcePolicy};
use std::fs;
use std::path::PathBuf;
use tiny_skia::Pixmap;
use url::Url;

fn get_pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y * pixmap.width() + x) as usize * 4;
  let data = pixmap.data();
  let a = data[idx + 3];
  if a == 0 {
    return (0, 0, 0, 0);
  }
  // tiny-skia uses premultiplied alpha.
  let r = ((data[idx] as u16 * 255) / a as u16) as u8;
  let g = ((data[idx + 1] as u16 * 255) / a as u16) as u8;
  let b = ((data[idx + 2] as u16 * 255) / a as u16) as u8;
  (r, g, b, a)
}

#[test]
fn dom_compatibility_lifts_lazy_loaded_images_at_render_time() {
  let fixture_dir =
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/fixtures/dom_compat_lazy_load");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_blue = |r: u8, g: u8, b: u8| b > 200 && r < 80 && g < 80;

  // Without compatibility mode the images should stay on their placeholders, so we shouldn't see
  // the red/blue SVGs.
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    !is_red(r, g, b),
    "standard render should not load the lazy img; got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 48);
  assert!(
    !is_blue(r, g, b),
    "standard render should not load the lazy picture source; got rgb({r},{g},{b})"
  );

  // With compatibility mode, the top half should be replaced by a red SVG (data-src) and the
  // bottom half by a blue SVG (<picture><source data-srcset>).
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should load the lazy img (red); got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 48);
  assert!(
    is_blue(r, g, b),
    "compat render should load the lazy picture source (blue); got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_scavenges_noscript_image_fallbacks_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_noscript_fallback");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_blue = |r: u8, g: u8, b: u8| b > 200 && r < 80 && g < 80;

  // Without compatibility mode, the images should remain on their placeholders because the real
  // sources only exist inside adjacent <noscript> markup.
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    !is_red(r, g, b),
    "standard render should not recover the <noscript> img (red); got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 48);
  assert!(
    !is_blue(r, g, b),
    "standard render should not recover the <noscript> picture source (blue); got rgb({r},{g},{b})"
  );

  // With compatibility mode, the sources are scavenged out of <noscript> fallbacks and copied into
  // the placeholders.
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should recover the <noscript> img (red); got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 48);
  assert!(
    is_blue(r, g, b),
    "compat render should recover the <noscript> picture source (blue); got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_lifts_data_default_src_images_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_data_attr_matrix");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 96))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 96))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_blue = |r: u8, g: u8, b: u8| b > 200 && r < 80 && g < 80;

  // Without compatibility mode, the first two images should remain placeholders (green background),
  // while the third image has an authored `src` and should still load.
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    !is_red(r, g, b),
    "standard render should not lift data-default-src; got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 48);
  assert!(
    !is_blue(r, g, b),
    "standard render should not lift data-src; got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 80);
  assert!(
    is_blue(r, g, b),
    "standard render should still load authored src (blue); got rgb({r},{g},{b})"
  );

  // With compatibility mode:
  // - The first row should be replaced by a red SVG (`data-default-src`).
  // - The second row should be replaced by a blue SVG (`data-src` remains higher priority).
  // - The third row remains blue (`src` is not overridden).
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should lift data-default-src (red); got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 48);
  assert!(
    is_blue(r, g, b),
    "compat render should lift data-src with higher priority (blue); got rgb({r},{g},{b})"
  );
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 80);
  assert!(
    is_blue(r, g, b),
    "compat render should preserve authored src (blue); got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_lifts_data_orig_file_images_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_img_orig_file");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_green = |r: u8, g: u8, b: u8| g > 200 && r < 80 && b < 80;

  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    is_green(r, g, b),
    "standard render should keep placeholder (green); got rgb({r},{g},{b})"
  );

  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should lift data-orig-file (red); got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_lifts_svg_placeholder_images_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_svg_placeholder");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 64))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_green = |r: u8, g: u8, b: u8| g > 200 && r < 80 && b < 80;

  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 32);
  assert!(
    is_green(r, g, b),
    "standard render should keep the SVG placeholder hidden; got rgb({r},{g},{b})"
  );

  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 32);
  assert!(
    is_red(r, g, b),
    "compat render should lift the real PNG from data-src; got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_lifts_video_posters_from_wrapper_data_attrs_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_video_poster_wrapper");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_green = |r: u8, g: u8, b: u8| g > 200 && r < 80 && b < 80;

  // Without compatibility mode the <video> has no poster, so it paints nothing and we see the
  // background.
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    is_green(r, g, b),
    "standard render should keep video transparent; got rgb({r},{g},{b})"
  );

  // With compatibility mode, the wrapper `data-poster-url` should be lifted into `<video poster>`
  // and rendered.
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should lift wrapper poster URL (red); got rgb({r},{g},{b})"
  );
}

#[test]
fn dom_compatibility_lifts_iframe_src_from_data_live_path_at_render_time() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_iframe_live_path");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let base_url = Url::from_directory_path(&fixture_dir)
    .expect("build file:// base url")
    .to_string();
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut standard = FastRender::builder()
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .dom_compatibility_mode(DomCompatibilityMode::Standard)
    .build()
    .expect("build standard renderer");

  let mut compat = FastRender::builder()
    .base_url(base_url)
    .resource_policy(policy)
    .dom_compatibility_mode(DomCompatibilityMode::Compatibility)
    .build()
    .expect("build compat renderer");

  let standard_pixmap = standard
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render standard fixture");
  let compat_pixmap = compat
    .render_html_with_options(&html, RenderOptions::new().with_viewport(64, 32))
    .expect("render compat fixture");

  let is_red = |r: u8, g: u8, b: u8| r > 200 && g < 80 && b < 80;
  let is_green = |r: u8, g: u8, b: u8| g > 200 && r < 80 && b < 80;

  // Without compatibility mode the `<iframe>` points at `about:blank`, which FastRender treats as
  // an empty transparent document.
  let (r, g, b, _) = get_pixel(&standard_pixmap, 32, 16);
  assert!(
    is_green(r, g, b),
    "standard render should keep iframe transparent; got rgb({r},{g},{b})"
  );

  // With compatibility mode, `data-live-path` should be lifted into `src` and rendered.
  let (r, g, b, _) = get_pixel(&compat_pixmap, 32, 16);
  assert!(
    is_red(r, g, b),
    "compat render should lift iframe data-live-path into src (red); got rgb({r},{g},{b})"
  );
}
