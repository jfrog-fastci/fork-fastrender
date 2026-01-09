use std::path::PathBuf;
use std::sync::Arc;

use fastrender::{ComputedStyle, FontContext, FontDatabase, ShapingPipeline};
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
fn helvetica_prefers_named_aliases_before_generic_sans_serif() {
  // Load DejaVu first so the fontdb generic `sans-serif` mapping resolves to DejaVu Sans, not
  // Noto Sans. This makes it possible to assert that our named-family alias expansion for
  // "Helvetica" is consulted before the generic fallback.
  let db = load_db_with_fonts(&[
    "tests/fixtures/fonts/DejaVuSans-subset.ttf",
    "tests/fixtures/fonts/NotoSans-subset.ttf",
  ]);

  let query = Query {
    families: &[Family::SansSerif],
    weight: Weight(400),
    stretch: Stretch::Normal,
    style: Style::Normal,
  };
  let sans_id = db.inner().query(&query).expect("sans-serif should resolve");
  let sans_font = db.load_font(sans_id).expect("sans-serif font should load");
  assert_eq!(
    sans_font.family, "DejaVu Sans",
    "test requires generic sans-serif to resolve to DejaVu Sans so we can observe aliasing"
  );

  let db = Arc::new(db);
  let ctx = FontContext::with_database(Arc::clone(&db));
  let mut style = ComputedStyle::default();
  style.font_family = vec!["Helvetica".to_string()].into();

  let runs = ShapingPipeline::new()
    .shape("Hello", &style, &ctx)
    .expect("shaping should succeed");
  assert!(!runs.is_empty(), "expected at least one shaped run");

  assert_eq!(
    runs[0].font.family, "Noto Sans",
    "Helvetica should alias to Noto Sans before falling back to the generic sans-serif mapping"
  );
}

