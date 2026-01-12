use crate::paint::text_rasterize::TextRasterizer;
use crate::style::color::Rgba;
use tiny_skia::{Color, Pixmap};

use super::color_font_helpers::{load_fixture_font, non_white_colors, shaped_run};

#[test]
fn font_size_zero_is_paint_noop() {
  let font = load_fixture_font("DejaVuSans-subset.ttf");
  let run = shaped_run(&font, 'A', 0.0, 0);

  let mut pixmap = Pixmap::new(32, 32).expect("pixmap");
  pixmap.fill(Color::WHITE);

  let mut rasterizer = TextRasterizer::new();
  rasterizer
    .render_shaped_run(&run, 0.0, 16.0, Rgba::BLACK, &mut pixmap)
    .expect("font-size 0 should not error during rasterization");

  assert_eq!(
    non_white_colors(&pixmap),
    0,
    "font-size 0 should not paint any pixels"
  );
}
