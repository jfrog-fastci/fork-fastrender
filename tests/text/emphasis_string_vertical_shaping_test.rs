use fastrender::paint::display_list::DisplayItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, RenderArtifactRequest, RenderArtifacts, RenderOptions};

fn emphasis_text_glyph_advances(html: &str) -> Vec<(f32, f32)> {
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

    let mut out = Vec::new();
    for run in &emphasis_text.runs {
      for glyph in &run.glyphs {
        out.push((glyph.x_advance, glyph.y_advance));
      }
    }
    return out;
  }

  panic!("expected a text item with string emphasis text");
}

#[test]
fn text_emphasis_string_uses_vertical_advances_in_vertical_typographic_modes() {
  // In vertical typographic modes, emphasis marks must remain upright even when the element uses
  // `text-orientation: sideways`. Ensure our emphasis string shaping uses vertical advances (y_advance)
  // so that the display list renderer can place and center the mark correctly along the inline axis.
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-rl; text-orientation: sideways; font-size:40px; line-height:1; text-emphasis-style: 'A';">
          A
        </div>
      </body>
    </html>
  "#;

  let advances = emphasis_text_glyph_advances(html);
  assert!(
    advances.iter().any(|(_, y)| y.abs() > 0.01),
    "expected emphasis string glyphs to use vertical advances (y_advance), got {advances:?}"
  );
}

