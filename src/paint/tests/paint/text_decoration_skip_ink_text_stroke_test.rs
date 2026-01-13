use crate::paint::display_list::DisplayItem;
use crate::paint::display_list::DisplayList;
use crate::text::font_db::FontConfig;
use crate::{
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

fn first_underline_segments(list: &DisplayList) -> Option<(f32, Vec<(f32, f32)>)> {
  for item in list.items() {
    let DisplayItem::TextDecoration(decoration) = item else {
      continue;
    };
    for paint in &decoration.decorations {
      let Some(stroke) = &paint.underline else {
        continue;
      };
      let segments = stroke
        .segments
        .clone()
        .unwrap_or_else(|| vec![(0.0, decoration.line_width)]);
      return Some((decoration.line_width, segments));
    }
  }
  None
}

fn sum_segment_lengths(segments: &[(f32, f32)]) -> f32 {
  segments.iter().map(|(s, e)| e - s).sum()
}

#[test]
fn underline_skip_ink_accounts_for_webkit_text_stroke_width() {
  let base_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          @font-face {
            font-family: "DejaVuSubset";
            src: url("tests/fixtures/fonts/DejaVuSans-subset.ttf") format("truetype");
          }
          body { margin: 0; background: white; }
          .t {
            font-family: "DejaVuSubset";
            font-size: 80px;
            line-height: 1;
            white-space: nowrap;
            color: black;
            text-decoration: underline;
            text-decoration-skip-ink: auto;
          }
        </style>
      </head>
      <body>
        <div class="t">aaaa g aaaa g aaaa</div>
      </body>
    </html>
  "#;

  let stroked_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          @font-face {
            font-family: "DejaVuSubset";
            src: url("tests/fixtures/fonts/DejaVuSans-subset.ttf") format("truetype");
          }
          body { margin: 0; background: white; }
          .t {
            font-family: "DejaVuSubset";
            font-size: 80px;
            line-height: 1;
            white-space: nowrap;
            color: black;
            text-decoration: underline;
            text-decoration-skip-ink: auto;
            -webkit-text-stroke: 4px black;
          }
        </style>
      </head>
      <body>
        <div class="t">aaaa g aaaa g aaaa</div>
      </body>
    </html>
  "#;

  let base_list = render_display_list(base_html, 1200, 200);
  let stroked_list = render_display_list(stroked_html, 1200, 200);

  let (base_width, base_segments) =
    first_underline_segments(&base_list).expect("expected underline segments for baseline");
  let (stroked_width, stroked_segments) =
    first_underline_segments(&stroked_list).expect("expected underline segments for stroked text");

  // Text stroke should not affect inline layout, so the decoration line width should match.
  assert!(
    (base_width - stroked_width).abs() < 0.1,
    "expected underline line width to match (base={base_width}, stroked={stroked_width})"
  );

  let base_sum = sum_segment_lengths(&base_segments);
  let stroked_sum = sum_segment_lengths(&stroked_segments);

  assert!(
    stroked_sum + 1.0 < base_sum,
    "expected text stroke to increase skip-ink carving (base_segments={base_segments:?} base_sum={base_sum}, stroked_segments={stroked_segments:?} stroked_sum={stroked_sum})"
  );
}
