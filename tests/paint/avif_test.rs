#![cfg(feature = "avif")]

use fastrender::FastRender;
use std::path::PathBuf;
use tiny_skia::Pixmap;
use url::Url;

fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn avif_fixture_url(name: &str) -> String {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/avif")
    .join(name);
  Url::from_file_path(path).unwrap().to_string()
}

fn page_fixture_url(path: &str) -> String {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
  Url::from_file_path(path).unwrap().to_string()
}

#[test]
fn background_image_set_renders_avif_candidate() {
  let mut renderer = FastRender::new().expect("renderer");
  let avif_url = avif_fixture_url("solid.avif");
  let webp_url = avif_fixture_url("solid.webp");

  let html = format!(
    r#"
    <style>
      body {{
        margin: 0;
        width: 8px;
        height: 8px;
        background-image: image-set(url("{avif}") 1x, url("{webp}") 2x);
        background-size: cover;
      }}
    </style>
	    <div style="width: 8px; height: 8px;"></div>
	    "#,
    avif = avif_url,
    webp = webp_url
  );

  let pixmap = renderer
    .render_html(&html, 8, 8)
    .expect("render avif background");
  let (r, g, b, _) = pixel_rgba(&pixmap, 4, 4);

  assert!(g > 170, "background should preserve green channel (g={g})");
  assert!(
    r < 60 && b < 60,
    "background should favor green over red/blue (r={r}, b={b})"
  );
}

#[test]
fn img_element_renders_avif_source() {
  let mut renderer = FastRender::new().expect("renderer");
  let avif_url = avif_fixture_url("solid.avif");

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      img {{ display: block; width: 8px; height: 8px; }}
    </style>
    <img src="{src}" alt="avif" />
    "#,
    src = avif_url
  );

  let pixmap = renderer
    .render_html(&html, 8, 8)
    .expect("render avif image");
  let (r, g, b, a) = pixel_rgba(&pixmap, 3, 3);

  assert_eq!(a, 255, "image pixels should be opaque");
  assert!(g > 170, "img element should decode avif (g={g})");
  assert!(
    r < 60 && b < 60,
    "img element should preserve green tint (r={r}, b={b})"
  );
}

#[test]
fn img_element_loading_lazy_renders_avif_source_when_in_viewport() {
  let mut renderer = FastRender::new().expect("renderer");
  let avif_url = avif_fixture_url("solid.avif");

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      img {{ display: block; width: 8px; height: 8px; }}
    </style>
    <img loading="lazy" src="{src}" alt="avif" />
    "#,
    src = avif_url
  );

  let pixmap = renderer
    .render_html(&html, 8, 8)
    .expect("render avif image (loading=lazy)");
  let (r, g, b, a) = pixel_rgba(&pixmap, 3, 3);

  assert_eq!(a, 255, "image pixels should be opaque");
  assert!(g > 170, "img element should decode avif (g={g})");
  assert!(
    r < 60 && b < 60,
    "img element should preserve green tint (r={r}, b={b})"
  );
}

#[test]
fn img_element_decodes_10bit_avif_into_non_black_pixels() {
  let mut renderer = FastRender::new().expect("renderer");
  let avif_url = page_fixture_url(
    "tests/pages/fixtures/gitlab.com/assets/22f6f99a9d5ab37d1feb616188f30106.avif",
  );

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      img {{ display: block; width: 768px; height: 432px; }}
    </style>
    <img src="{src}" alt="gitlab thumbnail" />
    "#,
    src = avif_url
  );

  let pixmap = renderer
    .render_html(&html, 768, 432)
    .expect("render gitlab avif");
  let mut max_channel = 0u8;
  for y in (0..pixmap.height()).step_by(37) {
    for x in (0..pixmap.width()).step_by(37) {
      let (r, g, b, a) = pixel_rgba(&pixmap, x, y);
      assert_eq!(a, 255, "decoded image pixels should be opaque (a={a})");
      max_channel = max_channel.max(r).max(g).max(b);
    }
  }
  assert!(
    max_channel > 15,
    "expected 10-bit AVIF to map into 8-bit channel range (max_channel={max_channel})"
  );
}

#[test]
fn img_element_decodes_avif_placeholder_thumbnail_into_non_black_pixels() {
  let mut renderer = FastRender::new().expect("renderer");
  let avif_url = page_fixture_url(
    "tests/pages/fixtures/gitlab.com/assets/8cc8f1857ff1da56e9d47f2ca8f3c43c.avif",
  );

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      img {{ display: block; width: 295px; height: 166px; }}
    </style>
    <img src="{src}" alt="gitlab placeholder" />
    "#,
    src = avif_url
  );

  let pixmap = renderer
    .render_html(&html, 295, 166)
    .expect("render placeholder avif");
  let (r, g, b, a) = pixel_rgba(&pixmap, 10, 10);

  assert_eq!(a, 255, "decoded image pixels should be opaque (a={a})");
  assert!(
    r > 0 || g > 0 || b > 0,
    "expected non-black pixels after AVIF decode (r={r}, g={g}, b={b})"
  );
}
