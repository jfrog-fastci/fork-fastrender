use crate::FontDatabase;

#[test]
fn bundled_fonts_report_missing_codepoint() {
  let db = FontDatabase::shared_bundled();
  assert!(
    !db.any_face_has_glyph_cached('\u{1AB0}'),
    "expected bundled fonts to be missing U+1AB0"
  );
}
