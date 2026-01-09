use fastrender::geometry::Rect;
use fastrender::paint::display_list::DecorationPaint;
use fastrender::paint::display_list::DecorationStroke;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list::TextDecorationItem;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::TextDecorationStyle;
use fastrender::text::font_db::FontConfig;
use fastrender::text::font_loader::FontContext;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions, Rgba,
};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y * pixmap.width() + x) as usize * 4;
  (
    pixmap.data()[idx],
    pixmap.data()[idx + 1],
    pixmap.data()[idx + 2],
    pixmap.data()[idx + 3],
  )
}

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
fn vertical_text_decoration_skip_ink_auto_carves_underline_segments() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; }
          .sample {
            writing-mode: vertical-rl;
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
    "expected at least one vertical TextDecorationItem in display list"
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

  let total_len: f32 = segments.iter().map(|(start, end)| end - start).sum();
  assert!(
    total_len < deco_item.line_width - 0.5,
    "expected vertical skip-ink to carve the underline (total_len={total_len}, line_width={}, segments={segments:?})",
    deco_item.line_width
  );
}

#[test]
fn display_list_vertical_skip_ink_dashed_preserves_dash_phase_across_segments() {
  // Equivalent to `tests/paint/display_list_skip_ink_test.rs` but for vertical underlines.
  let mut list = DisplayList::new();
  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 20.0, 90.0),
    line_start: 5.0,
    line_width: 80.0,
    inline_vertical: true,
    decorations: vec![DecorationPaint {
      style: TextDecorationStyle::Dashed,
      color: Rgba::BLACK,
      underline: Some(DecorationStroke {
        center: 10.0,
        thickness: 2.0,
        // Second segment starts at an offset that falls into the dash "off" region. If phase were
        // restarted (or incorrectly based on absolute coords), it would paint immediately.
        segments: Some(vec![(0.0, 25.0), (39.0, 80.0)]),
      }),
      overline: None,
      line_through: None,
    }],
  }));

  let pixmap = DisplayListRenderer::new(20, 90, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let is_whiteish = |p: (u8, u8, u8, u8)| p.0 > 240 && p.1 > 240 && p.2 > 240;
  let is_blackish = |p: (u8, u8, u8, u8)| p.0 < 32 && p.1 < 32 && p.2 < 32 && p.3 > 200;

  // First segment starts at y=line_start=5; ensure the dash paints near the start.
  assert!(
    is_blackish(pixel(&pixmap, 10, 6)),
    "expected dash to paint at y=6"
  );
  // With phase continuity, the absolute start of the second segment (y=5+39=44) lands inside the
  // dash gap and should remain white.
  assert!(
    is_whiteish(pixel(&pixmap, 10, 44)),
    "expected y=44 to be in an off-dash region when phase is preserved"
  );
  // Shortly after, the dash should resume.
  assert!(
    is_blackish(pixel(&pixmap, 10, 46)),
    "expected dash to paint at y=46"
  );
}

#[test]
fn display_list_vertical_skip_ink_wavy_preserves_wave_phase_across_segments() {
  let mut list = DisplayList::new();
  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 40.0, 140.0),
    line_start: 5.0,
    line_width: 120.0,
    inline_vertical: true,
    decorations: vec![DecorationPaint {
      style: TextDecorationStyle::Wavy,
      color: Rgba::BLACK,
      underline: Some(DecorationStroke {
        center: 20.0,
        // Use a thick stroke so the wave amplitude is large enough to assert on individual pixels.
        thickness: 8.0,
        // Second segment starts exactly one wavelength in (wavelength=thickness*4=32). A restarted
        // wave would start by bending the opposite way, flipping which side is painted at the
        // mid-point.
        segments: Some(vec![(0.0, 20.0), (32.0, 120.0)]),
      }),
      overline: None,
      line_through: None,
    }],
  }));

  let pixmap = DisplayListRenderer::new(40, 140, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let is_whiteish = |p: (u8, u8, u8, u8)| p.0 > 240 && p.1 > 240 && p.2 > 240;
  let is_blackish = |p: (u8, u8, u8, u8)| p.0 < 32 && p.1 < 32 && p.2 < 32 && p.3 > 200;

  // Mid-point of the first wavelength inside the second segment:
  //   abs_y = line_start + segment_start + wavelength/2 = 5 + 32 + 16 = 53
  // With preserved phase, the wave at this y should be on the *right* side of the centerline.
  let sample_y = 53u32;
  assert!(
    is_blackish(pixel(&pixmap, 26, sample_y)),
    "expected wavy underline to paint on the right side at y={sample_y}"
  );
  assert!(
    is_whiteish(pixel(&pixmap, 14, sample_y)),
    "expected left side to be unpainted at y={sample_y} when wave phase is preserved"
  );
}

