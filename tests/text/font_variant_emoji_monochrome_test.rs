use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use fastrender::style::types::FontVariantEmoji;
use fastrender::{ComputedStyle, FallbackChain, FontContext, FontDatabase, ShapingPipeline};

const MONO_EMOJI_FAMILY: &str = "Noto Sans Emoji 2";
const TEXT_SYMBOL_FAMILY: &str = "Noto Sans Symbols 2";
const TEST_CODEPOINT: char = '\u{2764}';

fn load_fixture_font_database() -> Arc<FontDatabase> {
  static DB: OnceLock<Arc<FontDatabase>> = OnceLock::new();
  Arc::clone(DB.get_or_init(|| {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut db = FontDatabase::empty();
    db.load_fonts_dir(manifest_dir.join("tests/fixtures/fonts"));
    Arc::new(db)
  }))
}

fn face_id_for_family(db: &FontDatabase, family: &str) -> fontdb::ID {
  db.faces()
    .find(|face| face.families.iter().any(|(name, _)| name == family))
    .map(|face| face.id)
    .unwrap_or_else(|| panic!("expected font face for family '{family}'"))
}

fn assert_fixture_coverage(db: &FontDatabase, emoji_face: fontdb::ID, text_face: fontdb::ID) {
  assert!(
    FontDatabase::is_emoji(TEST_CODEPOINT),
    "U+2764 should be treated as emoji for font-variant-emoji tests"
  );
  assert_eq!(
    db.is_color_capable_font(emoji_face),
    Some(false),
    "fixture emoji-named font should be monochrome (no color tables)"
  );
  assert!(
    db.has_glyph_cached(emoji_face, TEST_CODEPOINT),
    "fixture emoji-named font must cover U+2764"
  );
  assert!(
    db.has_glyph_cached(text_face, TEST_CODEPOINT),
    "fixture text font must cover U+2764"
  );
}

#[test]
fn font_variant_emoji_prefers_monochrome_emoji_family_even_if_text_font_is_first() {
  let db = load_fixture_font_database();
  let emoji_face = face_id_for_family(&db, MONO_EMOJI_FAMILY);
  let text_face = face_id_for_family(&db, TEXT_SYMBOL_FAMILY);
  assert_fixture_coverage(&db, emoji_face, text_face);

  let ctx = FontContext::with_database(Arc::clone(&db));
  let mut style = ComputedStyle::default();
  style.font_variant_emoji = FontVariantEmoji::Emoji;
  style.font_family = vec![
    TEXT_SYMBOL_FAMILY.to_string(),
    MONO_EMOJI_FAMILY.to_string(),
    "emoji".to_string(),
  ]
  .into();

  let runs = ShapingPipeline::new()
    .shape(&TEST_CODEPOINT.to_string(), &style, &ctx)
    .expect("shape emoji with fixture fonts");
  assert!(!runs.is_empty());
  assert_eq!(
    runs[0].font.family.as_str(),
    MONO_EMOJI_FAMILY,
    "font-variant-emoji:emoji should select the emoji-classified monochrome font"
  );
}

#[test]
fn font_variant_emoji_text_avoids_monochrome_emoji_family_even_if_it_is_first() {
  let db = load_fixture_font_database();
  let emoji_face = face_id_for_family(&db, MONO_EMOJI_FAMILY);
  let text_face = face_id_for_family(&db, TEXT_SYMBOL_FAMILY);
  assert_fixture_coverage(&db, emoji_face, text_face);

  let ctx = FontContext::with_database(Arc::clone(&db));
  let mut style = ComputedStyle::default();
  style.font_variant_emoji = FontVariantEmoji::Text;
  style.font_family = vec![MONO_EMOJI_FAMILY.to_string(), TEXT_SYMBOL_FAMILY.to_string()].into();

  let runs = ShapingPipeline::new()
    .shape(&TEST_CODEPOINT.to_string(), &style, &ctx)
    .expect("shape emoji with fixture fonts");
  assert!(!runs.is_empty());
  assert_eq!(
    runs[0].font.family.as_str(),
    TEXT_SYMBOL_FAMILY,
    "font-variant-emoji:text should avoid emoji fonts when a text alternative exists"
  );
}

#[test]
fn fallback_chain_resolve_emoji_scans_emoji_family_names_when_emoji_font_list_is_empty() {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  let mut db = FontDatabase::empty();
  for filename in [
    "tests/fixtures/fonts/NotoSansSymbols2-subset.ttf",
    "tests/fixtures/fonts/NotoSansSymbols2Emoji-subset.ttf",
  ] {
    let data = std::fs::read(manifest_dir.join(filename))
      .unwrap_or_else(|err| panic!("read fixture font {filename}: {err}"));
    db.load_font_data(data)
      .unwrap_or_else(|err| panic!("load fixture font {filename}: {err:?}"));
  }

  let emoji_face = face_id_for_family(&db, MONO_EMOJI_FAMILY);
  let text_face = face_id_for_family(&db, TEXT_SYMBOL_FAMILY);
  assert_fixture_coverage(&db, emoji_face, text_face);

  assert!(
    db.find_emoji_fonts().is_empty(),
    "fixture database contains no color emoji fonts, so the emoji list should be empty"
  );

  let chain = FallbackChain::new();
  let resolved = chain
    .resolve(TEST_CODEPOINT, &db)
    .expect("fallback chain should resolve emoji codepoint");
  assert_eq!(
    resolved.inner(),
    emoji_face,
    "fallback chain should use the emoji-named monochrome font for emoji codepoints"
  );
}
