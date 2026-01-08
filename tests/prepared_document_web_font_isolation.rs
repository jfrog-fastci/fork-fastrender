use fastrender::api::{FastRenderBuilder, RenderOptions};
use fastrender::text::font_db::FontConfig;
use std::path::PathBuf;
use url::Url;

fn fixtures_base_url() -> String {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  // Use a file URL (rather than a directory URL) so URL resolution via `Url::join` works even when
  // the base URL string does not end with a trailing slash.
  let document = root.join("tests/fixtures/accessibility/aria_relations.html");
  Url::from_file_path(document)
    .expect("fixtures base file:// URL")
    .to_string()
}

#[test]
fn prepared_document_web_fonts_are_isolated_from_subsequent_renders() {
  let base_url = fixtures_base_url();
  let options = RenderOptions::new().with_viewport(600, 200);

  let html_a = r#"<!doctype html>
<html>
  <head>
    <style>
      @font-face {
        font-family: "DocAFont";
        src: url("../fonts/Cantarell-Test.ttf") format("truetype");
        font-display: block;
      }
      html, body { margin: 0; background: #fff; }
      body { font-family: "DocAFont"; font-size: 96px; color: #000; }
    </style>
  </head>
  <body>FFFFF</body>
</html>"#;

  let html_b = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; background: #fff; }
      body { font-family: sans-serif; font-size: 96px; color: #000; }
    </style>
  </head>
  <body>SECOND</body>
</html>"#;

  let mut renderer = FastRenderBuilder::new()
    .base_url(base_url.clone())
    // Keep the system font set deterministic in CI so the fallback differs from DocAFont.
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let prepared_a = renderer
    .prepare_html(html_a, options.clone())
    .expect("prepare doc A");

  let first = prepared_a.paint_default().expect("paint doc A");
  let first_bytes = first.data().to_vec();

  // Render/prepare a second document to force the originating renderer to clear web fonts and
  // push/pop resource contexts. Previously this would mutate shared `Arc` state inside
  // `PreparedDocument` and break subsequent paints.
  let _ = renderer
    .prepare_html(html_b, options.clone())
    .expect("prepare doc B");

  let after_other_doc = prepared_a.paint_default().expect("paint doc A again");
  assert_eq!(
    first_bytes.as_slice(),
    after_other_doc.data(),
    "prepared document paint changed after another prepare/render call"
  );

  drop(renderer);
  let after_drop = prepared_a
    .paint_default()
    .expect("paint prepared doc after renderer drop");
  assert_eq!(
    first_bytes.as_slice(),
    after_drop.data(),
    "prepared document paint changed after renderer was dropped"
  );

  // Sanity: ensure the doc A paint differs from rendering the same text with the bundled
  // sans-serif fallback (so the test fails on the original bug where DocAFont would be cleared).
  let html_fallback = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; background: #fff; }
      body { font-family: sans-serif; font-size: 96px; color: #000; }
    </style>
  </head>
  <body>FFFFF</body>
</html>"#;

  let mut fallback_renderer = FastRenderBuilder::new()
    .base_url(base_url)
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("fallback renderer");
  let fallback = fallback_renderer
    .render_html_with_options(html_fallback, options)
    .expect("fallback render");
  assert_ne!(
    first_bytes.as_slice(),
    fallback.data(),
    "expected web-font render to differ from bundled sans-serif fallback"
  );
}
