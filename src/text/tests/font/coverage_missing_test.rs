#[test]
fn bundled_fonts_report_missing_codepoint() {
  use crate::{FontConfig, FontDatabase};

  // Ensure our bundled font set doesn't accidentally claim coverage for a codepoint we've seen as
  // missing in real-world content (regression test for pageset audits driven by `font_coverage`).
  let db = FontDatabase::with_config(&FontConfig::bundled_only());
  let ch = '\u{1AB0}';
  assert!(
    !db.any_face_has_glyph_cached(ch),
    "expected bundled fonts to be missing U+{:04X}",
    ch as u32
  );
}
