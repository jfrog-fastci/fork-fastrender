use crate::image_compare::CompareConfig;
use crate::image_output::{encode_image, OutputFormat};
use crate::style::color::Rgba;
use crate::testing::{compare_pixmaps, compare_pngs};
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
  let rendered_png = encode_image(colored.image.as_ref(), OutputFormat::Png).expect("encode png");
  if std::env::var("UPDATE_GOLDEN").is_ok() {
    std::fs::write(&golden_path, &rendered_png).expect("write golden");
  }

  let expected = std::fs::read(&golden_path).expect("load golden");
  let diff_dir =
    crate::testing::manifest_dir().join("target/test-artifacts/paint/svg_context_paint_font");
  compare_pngs(
    "svg_context_paint_font",
    &rendered_png,
    &expected,
    &CompareConfig::strict(),
    &diff_dir,
  )
  .unwrap_or_else(|e| panic!("{e}"));

  let color_diff =
    compare_pixmaps(colored.image.as_ref(), black.image.as_ref(), &CompareConfig::strict());
  assert!(
    !color_diff.is_match(),
    "context paint should change when text color changes"
  );
}
