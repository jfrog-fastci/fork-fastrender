use fastrender::style::media::MediaContext;
use fastrender::text::font_db::{FontStretch, FontStyle};
use fastrender::text::font_loader::{FontContext, FontLoadStatus, WebFontLoadOptions};
use std::path::Path;
use url::Url;

#[test]
fn weibo_fixture_font_face_relative_url_resolves_against_document_base_url() {
  // The `weibo.cn` page fixture declares a CJK font using `@font-face` inside an inline `<style>`
  // block. Inline styles do not have a stylesheet URL, so `url(...)` sources must resolve against
  // the document URL passed as `base_url` to `FontContext::load_web_fonts`.
  let doc_path = Path::new("tests/pages/fixtures/weibo.cn/index.html");
  let font_path = Path::new("tests/fixtures/fonts/NotoSansSC-subset.ttf");
  if !(doc_path.exists() && font_path.exists()) {
    return;
  }

  let doc_url = Url::from_file_path(doc_path.canonicalize().expect("canonical fixture path"))
    .expect("file URL for fixture");
  let expected_font_url =
    Url::from_file_path(font_path.canonicalize().expect("canonical font path"))
      .expect("file URL for font");

  let css = r#"
    @font-face {
      font-family: "WeiboCJKTest";
      src: url("../../../fixtures/fonts/NotoSansSC-subset.ttf") format("truetype");
      font-weight: 400;
      font-style: normal;
      font-display: block;
    }
  "#;
  let sheet = fastrender::css::parser::parse_stylesheet(css).expect("stylesheet should parse");
  let media_ctx = MediaContext::screen(800.0, 600.0);
  let faces = sheet.collect_font_face_rules(&media_ctx);
  assert_eq!(faces.len(), 1);

  let ctx = FontContext::empty();
  let report = ctx
    .load_web_fonts_with_options(
      &faces,
      Some(doc_url.as_str()),
      None,
      WebFontLoadOptions::default(),
    )
    .expect("load web font");
  assert!(
    report.events.iter().any(|event| {
      matches!(event.status, FontLoadStatus::Loaded)
        && event.source.as_deref() == Some(expected_font_url.as_str())
    }),
    "expected relative @font-face src URL to resolve against document base URL (events={:?})",
    report.events
  );

  // The loaded face should be resolved by the family name (i.e. active web fonts win).
  let families = vec!["WeiboCJKTest".to_string()];
  let font = ctx
    .get_font_full(&families, 400, FontStyle::Normal, FontStretch::Normal)
    .expect("web font should be available");
  assert_eq!(font.family, "WeiboCJKTest");
}
