use std::path::PathBuf;

use fastrender::{
  FallbackChain, FontConfig, FontDatabase, FontStyleDb as FontStyle, FontWeightDb as FontWeight,
  GenericFamily,
};
use fontdb::{Family, Query, Stretch, Style, Weight};

fn load_db_with_fonts(paths: &[&str]) -> FontDatabase {
  let mut db = FontDatabase::empty();
  for path in paths {
    let data = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
      .expect("test font should load");
    db.load_font_data(data).expect("font should parse");
  }
  db.refresh_generic_fallbacks();
  db
}

#[test]
fn sans_serif_prefers_known_family_over_first_loaded_font() {
  let db = load_db_with_fonts(&[
    "tests/fonts/ColorTestCOLR.ttf",
    "tests/fixtures/fonts/DejaVuSans-subset.ttf",
  ]);

  let deja_vu_id = db
    .query("DejaVu Sans", FontWeight::NORMAL, FontStyle::Normal)
    .expect("DejaVu Sans should be available");
  let color_id = db
    .query("ColorTestCOLR", FontWeight::NORMAL, FontStyle::Normal)
    .expect("ColorTestCOLR should be available");

  let query = Query {
    families: &[Family::SansSerif],
    weight: Weight(400),
    stretch: Stretch::Normal,
    style: Style::Normal,
  };
  let sans_id = db.inner().query(&query).expect("sans-serif should resolve");

  assert_eq!(sans_id, deja_vu_id);
  assert_ne!(sans_id, color_id);
}

#[test]
fn system_ui_uses_named_fallbacks_before_generic_mapping() {
  let db = load_db_with_fonts(&[
    "tests/fonts/ColorTestCOLR.ttf",
    "tests/fixtures/fonts/DejaVuSans-subset.ttf",
    "tests/fixtures/fonts/Cantarell-Test.ttf",
  ]);

  let cantarell_id = db
    .query("Cantarell", FontWeight::NORMAL, FontStyle::Normal)
    .expect("Cantarell fixture should be available");

  let chain = FallbackChain::new().add_generic(GenericFamily::SystemUi);

  let default_id = chain
    .resolve_default(&db)
    .expect("system-ui should resolve a default font")
    .inner();
  assert_eq!(default_id, cantarell_id);

  let resolved = chain
    .resolve('F', &db)
    .expect("system-ui should resolve glyph coverage")
    .inner();
  assert_eq!(resolved, cantarell_id);
}

#[test]
fn ui_sans_serif_prefers_noto_sans_over_dejavu_when_both_available() {
  let db = load_db_with_fonts(&[
    "tests/fixtures/fonts/DejaVuSans-subset.ttf",
    "tests/fixtures/fonts/NotoSans-subset.ttf",
  ]);

  let noto_id = db
    .query("Noto Sans", FontWeight::NORMAL, FontStyle::Normal)
    .expect("Noto Sans should be available");
  let dejavu_id = db
    .query("DejaVu Sans", FontWeight::NORMAL, FontStyle::Normal)
    .expect("DejaVu Sans should be available");

  let chain = FallbackChain::new().add_generic(GenericFamily::UiSansSerif);
  let default_id = chain
    .resolve_default(&db)
    .expect("ui-sans-serif should resolve a default font")
    .inner();

  assert_eq!(default_id, noto_id);
  assert_ne!(default_id, dejavu_id);
}

#[test]
fn emoji_generic_prefers_bundled_emoji_face_in_bundled_only_mode() {
  let db = FontDatabase::with_config(&FontConfig::bundled_only());
  let chain = FallbackChain::new().add_generic(GenericFamily::Emoji);

  // `font-family: emoji` is frequently queried before the specific characters are known (e.g. to
  // establish default font metrics). In bundled-only mode the deterministic emoji face should be
  // preferred over `fontdb`'s sans-serif generic mapping.
  let default_id = chain
    .resolve_default(&db)
    .expect("emoji generic should resolve a default font")
    .inner();
  let default_font = db.load_font(default_id).expect("resolved font should load");
  assert_eq!(default_font.family, "FastRender Emoji");

  // Emoji characters should resolve to the bundled emoji face when present.
  let emoji_id = chain
    .resolve('💩', &db)
    .expect("emoji should resolve")
    .inner();
  let emoji_font = db.load_font(emoji_id).expect("resolved font should load");
  assert_eq!(emoji_font.family, "FastRender Emoji");

  // Non-emoji characters should still resolve, falling back to a text face when needed.
  let text_id = chain
    .resolve('A', &db)
    .expect("non-emoji should resolve")
    .inner();
  let text_font = db.load_font(text_id).expect("resolved font should load");
  assert_ne!(text_font.family, "");
  assert_ne!(text_font.family, "FastRender Emoji");
}
