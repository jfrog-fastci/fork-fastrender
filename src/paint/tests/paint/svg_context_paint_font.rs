use crate::image_compare::{compare_png, CompareConfig};
use crate::image_output::{encode_image, OutputFormat};
use crate::style::color::Rgba;
use crate::text::color_fonts::ColorFontRenderer;
use crate::text::font_db::FontDatabase;
use crate::text::font_instance::FontInstance;
use std::path::PathBuf;

#[test]
fn svg_context_paint_uses_text_color() {
  let font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/fonts/DejaVuSans-context-paint.ttf");
  let bytes = std::fs::read(&font_path).expect("font bytes");

  let mut db = FontDatabase::empty();
  db.load_font_data(bytes).expect("load font");
  let font = db.first_font().expect("loaded font");
  let face = font.as_ttf_face().expect("parse font face");
  let glyph_id = face.glyph_index('F').expect("glyph id").0 as u32;

  let renderer = ColorFontRenderer::new();
  let text_color = Rgba::from_rgba8(0, 180, 220, 255);
  let instance = FontInstance::new(&font, &[]).expect("font instance");
  let colored = renderer
    .render(
      &font,
      &instance,
      glyph_id,
      64.0,
      0,
      &[],
      0,
      text_color,
      0.0,
      &[],
      None,
    )
    .expect("svg glyph");
  let black = renderer
    .render(
      &font,
      &instance,
      glyph_id,
      64.0,
      0,
      &[],
      0,
      Rgba::BLACK,
      0.0,
      &[],
      None,
    )
    .expect("svg glyph");

  let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/golden/svg_context_paint_font.png");
  if std::env::var("UPDATE_GOLDEN").is_ok() {
    let colored_png = encode_image(colored.image.as_ref(), OutputFormat::Png).expect("encode png");
    std::fs::write(&golden_path, colored_png).expect("write golden");
    return;
  }

  let colored_png = encode_image(colored.image.as_ref(), OutputFormat::Png).expect("encode png");
  let expected_png = std::fs::read(&golden_path).expect("load golden");
  let diff =
    compare_png(&colored_png, &expected_png, &CompareConfig::strict()).expect("compare pngs");
  assert!(
    diff.is_match(),
    "context-fill/stroke should resolve to text color: {}",
    diff.summary()
  );

  let black_png = encode_image(black.image.as_ref(), OutputFormat::Png).expect("encode png");
  let color_diff =
    compare_png(&colored_png, &black_png, &CompareConfig::strict()).expect("compare pngs");
  assert!(
    !color_diff.is_match(),
    "context paint should change when text color changes"
  );
}
