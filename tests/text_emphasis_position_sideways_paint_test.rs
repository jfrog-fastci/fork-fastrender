use fastrender::paint::display_list::DisplayItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, RenderArtifactRequest, RenderArtifacts, RenderOptions};

fn emphasis_mark_delta_x(html: &str) -> f32 {
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
    assert!(
      emphasis.inline_vertical,
      "expected vertical emphasis marks, got inline_vertical=false"
    );
    let mark = emphasis.marks.first().expect("expected at least one mark");
    return mark.center.x - text.origin.x;
  }

  panic!("expected a text item with emphasis marks");
}

#[test]
fn text_emphasis_position_ignores_left_right_in_sideways_writing_mode() {
  // In horizontal typographic modes, `text-emphasis-position` is controlled by `over`/`under`;
  // the `left`/`right` keywords are only meaningful in vertical typographic modes.
  //
  // `writing-mode: sideways-rl` is a horizontal typographic mode of a vertical writing mode.
  let html_left = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: sideways-rl; font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: under left;">
          A
        </div>
      </body>
    </html>
  "#;
  let html_right = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: sideways-rl; font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: under right;">
          A
        </div>
      </body>
    </html>
  "#;

  let left = emphasis_mark_delta_x(html_left);
  let right = emphasis_mark_delta_x(html_right);
  assert!(
    (left - right).abs() < 0.01,
    "expected left/right to be ignored in sideways writing modes, got left={left} right={right}"
  );
}

