use fastrender::debug::runtime::RuntimeToggles;
use fastrender::geometry::Rect;
use fastrender::paint::display_list::{DisplayItem, DisplayList};
use fastrender::style::color::SystemColor;
use fastrender::text::font_db::FontConfig;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions, Rgba,
};
use std::collections::HashMap;

fn approx_eq(a: f32, b: f32) -> bool {
  (a - b).abs() < 0.01
}

fn render_display_list_forced_colors(html: &str, width: u32, height: u32) -> DisplayList {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
    ("FASTR_FORCED_COLORS".to_string(), "active".to_string()),
  ]));

  // `render_html_with_options_and_artifacts` installs runtime toggles from `RenderOptions` / renderer
  // config. Use the renderer config so forced-colors is active in the full render pipeline.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles)
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts.display_list.take().expect("display list")
}

fn find_unique_fill_rect(list: &DisplayList, color: Rgba, width: f32, height: f32) -> Rect {
  let mut rect: Option<Rect> = None;
  for item in list.items() {
    let candidate = match item {
      DisplayItem::FillRect(fill)
        if fill.color == color && approx_eq(fill.rect.width(), width) && approx_eq(fill.rect.height(), height) =>
      {
        Some(fill.rect)
      }
      DisplayItem::FillRoundedRect(fill)
        if fill.color == color && approx_eq(fill.rect.width(), width) && approx_eq(fill.rect.height(), height) =>
      {
        Some(fill.rect)
      }
      _ => None,
    };
    if let Some(candidate) = candidate {
      assert!(
        rect.is_none(),
        "expected unique {width}x{height} fill rect for {color:?}, found multiple"
      );
      rect = Some(candidate);
    }
  }
  rect.expect("expected fill rect not found")
}

fn non_empty_text_colors(list: &DisplayList) -> Vec<Rgba> {
  list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) if !text.glyphs.is_empty() => Some(text.color),
      _ => None,
    })
    .collect()
}

#[test]
fn forced_colors_overrides_authored_colors_in_display_list() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; }
          #t {
            width: 20px;
            height: 20px;
            background-color: rgb(255, 0, 0);
            color: rgb(0, 0, 255);
            font-size: 16px;
          }
        </style>
      </head>
      <body>
        <div id="t">X</div>
      </body>
    </html>
  "#;

  let list = render_display_list_forced_colors(html, 50, 30);

  let canvas = SystemColor::Canvas.to_rgba(false, true);
  let canvas_text = SystemColor::CanvasText.to_rgba(false, true);

  find_unique_fill_rect(&list, canvas, 20.0, 20.0);

  let colors = non_empty_text_colors(&list);
  assert!(!colors.is_empty(), "expected at least one text run");
  assert!(
    colors.iter().all(|c| *c == canvas_text),
    "expected all text runs to use forced CanvasText {canvas_text:?}, got {colors:?}"
  );
}

#[test]
fn forced_color_adjust_none_preserves_authored_colors_in_display_list() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; }
          #t {
            width: 20px;
            height: 20px;
            forced-color-adjust: none;
            background-color: rgb(255, 0, 0);
            color: rgb(0, 0, 255);
            font-size: 16px;
          }
        </style>
      </head>
      <body>
        <div id="t">X</div>
      </body>
    </html>
  "#;

  let list = render_display_list_forced_colors(html, 50, 30);

  let authored_bg = Rgba::rgb(255, 0, 0);
  let authored_text = Rgba::rgb(0, 0, 255);

  find_unique_fill_rect(&list, authored_bg, 20.0, 20.0);

  let colors = non_empty_text_colors(&list);
  assert!(!colors.is_empty(), "expected at least one text run");
  assert!(
    colors.iter().all(|c| *c == authored_text),
    "expected all text runs to preserve authored color {authored_text:?}, got {colors:?}"
  );
}
