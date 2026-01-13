use fastrender::paint::display_list::{
  DecorationPaint, DecorationStroke, DisplayItem, DisplayList, TextDecorationItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::TextDecorationStyle;
use fastrender::text::font_loader::FontContext;
use fastrender::{Rect, Rgba};
use tiny_skia::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = ((y * pixmap.width() + x) * 4) as usize;
  let data = pixmap.data();
  (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
}

#[test]
fn text_decoration_solid_snaps_fractional_thickness_to_device_pixels() {
  let mut list = DisplayList::new();
  list.push(DisplayItem::TextDecoration(TextDecorationItem {
    bounds: Rect::from_xywh(0.0, 0.0, 20.0, 15.0),
    line_start: 0.0,
    line_width: 20.0,
    inline_vertical: false,
    decorations: vec![
      DecorationPaint {
        style: TextDecorationStyle::Solid,
        color: Rgba::from_rgba8(0, 0, 255, 255),
        underline: Some(DecorationStroke {
          // pre-snap edge = 6.8 - 0.9 = 5.9, so snap edge to 6 and thickness to 1.
          center: 6.8,
          thickness: 1.8,
          segments: None,
        }),
        overline: None,
        line_through: None,
      },
      DecorationPaint {
        style: TextDecorationStyle::Solid,
        color: Rgba::from_rgba8(255, 0, 0, 255),
        underline: Some(DecorationStroke {
          // pre-snap edge = 10.8 - 1.35 = 9.45, so snap edge to 9 and thickness to 2.
          center: 10.8,
          thickness: 2.7,
          segments: None,
        }),
        overline: None,
        line_through: None,
      },
    ],
  }));

  let pixmap = DisplayListRenderer::new(20, 15, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("rendered");

  // 1.8px thickness should snap down to a crisp 1px line at y=6.
  assert_eq!(pixel(&pixmap, 10, 6), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 10, 5), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 10, 7), (255, 255, 255, 255));
  // 2.7px thickness should snap down to a crisp 2px line at y=9..10.
  assert_eq!(pixel(&pixmap, 10, 9), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 10, 11), (255, 255, 255, 255));
}

