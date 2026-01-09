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
fn text_emphasis_under_right_paints_on_right_in_vertical_rl() {
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-rl; font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: under right;">
          A
        </div>
      </body>
    </html>
  "#;
  assert!(
    emphasis_mark_delta_x(html) > 0.0,
    "expected emphasis mark center to be on the right of the baseline in vertical-rl"
  );
}

#[test]
fn text_emphasis_under_left_paints_on_left_in_vertical_rl() {
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-rl; font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: under left;">
          A
        </div>
      </body>
    </html>
  "#;
  assert!(
    emphasis_mark_delta_x(html) < 0.0,
    "expected emphasis mark center to be on the left of the baseline in vertical-rl"
  );
}

#[test]
fn text_emphasis_under_right_paints_on_right_in_vertical_lr() {
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-lr; font-size:40px; line-height:1; text-emphasis-style: dot; text-emphasis-position: under right;">
          A
        </div>
      </body>
    </html>
  "#;
  assert!(
    emphasis_mark_delta_x(html) > 0.0,
    "expected emphasis mark center to be on the right of the baseline in vertical-lr"
  );
}

