use fastrender::text::font_db::{FontConfig, FontDatabase, FontStyle, FontWeight};

#[test]
fn generic_fallbacks_prefer_non_bundled_faces_when_both_available() {
  // Integration-level regression for `FontDatabase::set_generic_fallbacks`: when callers load
  // bundled fallback fonts alongside their own font directories (e.g. fixture/chrome runs that
  // enable system fonts), generic family mapping should prefer non-bundled candidates so text
  // metrics match what Chrome resolves via system font configuration.
  //
  // We simulate this by:
  // - Loading a non-bundled DejaVu Sans into a temporary font directory.
  // - Enabling bundled fonts (which includes bundled Noto Sans).
  //
  // The `SansSerif` fallback list prefers Noto Sans over DejaVu Sans, but if the only available
  // Noto Sans is the bundled one we should still pick the non-bundled DejaVu Sans.
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let font_path = temp_dir.path().join("DejaVuSans-subset.ttf");
  std::fs::write(
    &font_path,
    include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf"),
  )
  .expect("write DejaVu Sans subset");

  let config = FontConfig {
    use_system_fonts: false,
    use_bundled_fonts: true,
    font_dirs: vec![temp_dir.path().to_path_buf()],
  };
  let db = FontDatabase::with_config(&config);

  let id = db
    .query("sans-serif", FontWeight::NORMAL, FontStyle::Normal)
    .expect("resolve sans-serif");
  let font = db.load_font(id).expect("load resolved sans-serif");
  assert_eq!(font.family, "DejaVu Sans");
}

