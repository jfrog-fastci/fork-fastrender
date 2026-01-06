use std::path::Path;

use fastrender::text::font_db::FontDatabase;

#[test]
fn find_emoji_fonts_includes_family_name_heuristics_even_without_color_tables() {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

  let mut db = FontDatabase::empty();
  db.load_fonts_dir(manifest_dir.join("tests/fixtures/fonts"));

  let monochrome_emoji_id = db
    .faces()
    .find(|face| {
      face
        .families
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("EmojiFont11"))
    })
    .map(|face| face.id)
    .expect("EmojiFont11 fixture face should be discoverable via fontdb metadata");

  assert_eq!(
    db.is_color_capable_font(monochrome_emoji_id),
    Some(false),
    "EmojiFont11 fixture should have no color tables"
  );

  let emoji_fonts = db.find_emoji_fonts();
  assert!(
    emoji_fonts.contains(&monochrome_emoji_id),
    "emoji font list should include monochrome emoji fonts detected by family-name heuristics"
  );
}

