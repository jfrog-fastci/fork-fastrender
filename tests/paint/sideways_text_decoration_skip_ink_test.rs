use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::DisplayList;
use fastrender::text::font_db::FontConfig;
use fastrender::text::pipeline::RunRotation;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions,
};

fn render_display_list(html: &str, width: u32, height: u32) -> DisplayList {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
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

#[test]
fn sideways_text_decoration_skip_ink_auto_carves_underline_segments() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; }
          .sample {
            writing-mode: sideways-lr;
            font-size: 96px;
            line-height: 1;
            /* Force the underline band to overlap the glyph bounds so skip-ink has work to do. */
            text-decoration: underline;
            text-decoration-skip-ink: auto;
            text-decoration-thickness: 12px;
            text-underline-offset: -0.15em;
            white-space: pre;
          }
        </style>
      </head>
      <body>
        <div class="sample">g g</div>
      </body>
    </html>
  "#;

  let list = render_display_list(html, 280, 420);

  let rotated_text: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) if text.rotation != RunRotation::None && !text.glyphs.is_empty() => {
        Some(text)
      }
      _ => None,
    })
    .collect();
  assert!(
    !rotated_text.is_empty(),
    "expected sideways writing-mode to produce rotated Text items"
  );

  let decorations: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TextDecoration(item) if item.inline_vertical => Some(item),
      _ => None,
    })
    .collect();
  assert!(
    !decorations.is_empty(),
    "expected at least one inline-vertical TextDecorationItem in display list"
  );

  let deco_item = decorations[0];
  let underline = deco_item
    .decorations
    .iter()
    .find_map(|deco| deco.underline.as_ref())
    .expect("expected underline stroke");
  let segments = underline
    .segments
    .as_ref()
    .expect("expected underline segments for skip-ink auto");

  // Sideways writing modes paint rotated runs. If the skip-ink builder were to ignore the run's
  // rotation transform, all glyph AABBs would overlap at the same inline position (y=0), which
  // would produce a single underline segment instead of splitting around glyphs.
  assert!(
    segments.len() > 1,
    "expected skip-ink to carve multiple underline segments for sideways writing-mode; got segments={segments:?} (line_width={})",
    deco_item.line_width
  );

  let total_len: f32 = segments.iter().map(|(start, end)| end - start).sum();
  assert!(
    total_len < deco_item.line_width - 5.0,
    "expected sideways skip-ink to carve the underline (total_len={total_len}, line_width={}, segments={segments:?})",
    deco_item.line_width
  );
}
