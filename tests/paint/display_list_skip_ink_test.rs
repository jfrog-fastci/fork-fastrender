use fastrender::geometry::Rect;
use fastrender::paint::display_list::DecorationPaint;
use fastrender::paint::display_list::DecorationStroke;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::TextDecorationItem;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::TextDecorationStyle;
use fastrender::text::font_loader::FontContext;
use fastrender::Rgba;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y * pixmap.width() + x) as usize * 4;
  (
    pixmap.data()[idx],
    pixmap.data()[idx + 1],
    pixmap.data()[idx + 2],
    pixmap.data()[idx + 3],
  )
}

#[test]
fn display_list_skip_ink_none_renders_full_line() {
  let mut list = fastrender::paint::display_list::DisplayList::new();
  let decoration = DecorationPaint {
    style: TextDecorationStyle::Solid,
    color: Rgba::BLACK,
    underline: Some(DecorationStroke {
      center: 5.0,
      thickness: 2.0,
      segments: None,
    }),
    overline: None,
    line_through: None,
  };

  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    line_start: 0.0,
    line_width: 10.0,
    decorations: vec![decoration.clone()],
    inline_vertical: false,
  }));

  let pixmap = DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Underline painted through the center; check a mid-line pixel is black.
  assert_eq!(pixel(&pixmap, 5, 5), (0, 0, 0, 255));
}

#[test]
fn display_list_skip_ink_all_carves_out_segment() {
  // Simulate skip-ink carving by providing a segments entry.
  let mut list = fastrender::paint::display_list::DisplayList::new();
  let decoration = DecorationPaint {
    style: TextDecorationStyle::Solid,
    color: Rgba::BLACK,
    underline: Some(DecorationStroke {
      center: 5.0,
      thickness: 2.0,
      segments: Some(vec![(3.0, 7.0)]),
    }),
    overline: None,
    line_through: None,
  };

  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    line_start: 0.0,
    line_width: 10.0,
    decorations: vec![decoration],
    inline_vertical: false,
  }));

  let pixmap = DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Only the provided segment is painted; outside the segment stays white.
  assert_eq!(pixel(&pixmap, 4, 5), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 1, 5), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 8, 5), (255, 255, 255, 255));
}

#[test]
fn display_list_skip_ink_segments_respect_line_start() {
  // Segments are stored relative to `line_start`; the renderer must offset them when painting.
  let mut list = fastrender::paint::display_list::DisplayList::new();
  let decoration = DecorationPaint {
    style: TextDecorationStyle::Solid,
    color: Rgba::BLACK,
    underline: Some(DecorationStroke {
      center: 5.0,
      thickness: 2.0,
      segments: Some(vec![(0.0, 10.0)]),
    }),
    overline: None,
    line_through: None,
  };

  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 20.0, 10.0),
    line_start: 5.0,
    line_width: 10.0,
    decorations: vec![decoration],
    inline_vertical: false,
  }));

  let pixmap = DisplayListRenderer::new(20, 10, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Underline should start at x = line_start (5px), not at 0.
  assert_eq!(pixel(&pixmap, 7, 5), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 2, 5), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 17, 5), (255, 255, 255, 255));
}

#[test]
fn display_list_skip_ink_dashed_preserves_dash_phase_across_segments() {
  // When skip-ink splits a dashed decoration into multiple segments, the dash phase should
  // continue as if it were a single continuous stroke (matching browser behavior).
  let mut list = fastrender::paint::display_list::DisplayList::new();
  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 80.0, 10.0),
    line_start: 0.0,
    line_width: 80.0,
    decorations: vec![DecorationPaint {
      style: TextDecorationStyle::Dashed,
      color: Rgba::BLACK,
      underline: Some(DecorationStroke {
        center: 5.0,
        thickness: 2.0,
        // Start the second segment at an offset that falls into the dash "off" region, so a
        // restarted pattern would incorrectly paint immediately.
        segments: Some(vec![(0.0, 25.0), (39.0, 80.0)]),
      }),
      overline: None,
      line_through: None,
    }],
    inline_vertical: false,
  }));

  let pixmap = DisplayListRenderer::new(80, 10, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let is_whiteish = |p: (u8, u8, u8, u8)| p.0 > 240 && p.1 > 240 && p.2 > 240;
  let is_blackish = |p: (u8, u8, u8, u8)| p.0 < 32 && p.1 < 32 && p.2 < 32 && p.3 > 200;

  assert!(
    is_blackish(pixel(&pixmap, 1, 5)),
    "expected dash to paint at x=1"
  );
  // With phase continuity, x=39 falls into the dash gap (off) region and should remain white.
  assert!(
    is_whiteish(pixel(&pixmap, 39, 5)),
    "expected x=39 to be in an off-dash region when phase is preserved"
  );
  // Shortly after, the dash should resume.
  assert!(
    is_blackish(pixel(&pixmap, 41, 5)),
    "expected dash to paint at x=41"
  );
}
