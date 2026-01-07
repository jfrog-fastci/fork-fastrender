use fastrender::paint::display_list::DisplayItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, RenderArtifactRequest, RenderArtifacts, RenderOptions};

fn emphasis_mark_count(html: &str) -> usize {
  let font_config = FontConfig::default()
    .with_system_fonts(false)
    .with_bundled_fonts(true);
  let mut renderer = FastRender::builder()
    .font_sources(font_config)
    .build()
    .expect("renderer");
  let options = RenderOptions::new().with_viewport(200, 200);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..Default::default()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  let display_list = artifacts
    .display_list
    .take()
    .expect("display list captured");

  display_list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => text.emphasis.as_ref(),
      _ => None,
    })
    .map(|emphasis| emphasis.marks.len())
    .sum()
}

#[test]
fn text_emphasis_skips_period_punctuation() {
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: over;">
          A.A
        </div>
      </body>
    </html>
  "#;
  assert_eq!(
    emphasis_mark_count(html),
    2,
    "expected emphasis marks only for letters, not for '.' punctuation"
  );
}

#[test]
fn text_emphasis_keeps_allowed_hash_symbol() {
  // CSS Text Decoration 4 allows emphasis marks for punctuation that normalizes to
  // U+0023 NUMBER SIGN (#).
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: over;">
          A#A
        </div>
      </body>
    </html>
  "#;
  assert_eq!(
    emphasis_mark_count(html),
    3,
    "expected emphasis marks for '#', matching the spec allowlist"
  );
}

