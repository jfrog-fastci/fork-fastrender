use fastrender::paint::display_list::DisplayItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, RenderArtifactRequest, RenderArtifacts, RenderOptions};
use unicode_segmentation::UnicodeSegmentation;

fn emphasis_text_glyph_clusters(html: &str) -> Vec<u32> {
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

  for item in display_list.items() {
    let DisplayItem::Text(text) = item else {
      continue;
    };
    let Some(emphasis) = &text.emphasis else {
      continue;
    };
    let Some(emphasis_text) = &emphasis.text else {
      continue;
    };

    let mut clusters = Vec::new();
    for run in &emphasis_text.runs {
      for glyph in &run.glyphs {
        clusters.push(glyph.cluster);
      }
    }
    return clusters;
  }

  panic!("expected a text item with string emphasis text");
}

#[test]
fn text_emphasis_string_truncates_to_first_grapheme_cluster() {
  // CSS Text Decoration 4: the UA may truncate or ignore strings consisting of more than one
  // grapheme cluster. We truncate to the first cluster, matching browser behavior.
  let mark = format!("A\u{0301}B");
  let first_cluster_len = mark.graphemes(true).next().expect("cluster").len() as u32;
  let html = format!(
    r#"
      <html>
        <body style="margin:0">
          <div style="font-size:40px; line-height:1; text-emphasis-style: '{mark}';">
            A
          </div>
        </body>
      </html>
    "#,
    mark = mark
  );

  let clusters = emphasis_text_glyph_clusters(&html);
  assert!(
    clusters.iter().all(|&cluster| cluster < first_cluster_len),
    "expected emphasis string glyph clusters {clusters:?} to be within the first grapheme cluster (byte len {first_cluster_len})"
  );
}

