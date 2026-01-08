use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use std::sync::Arc;

const DEJAVU_SANS: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");

fn dejavu_sans() -> LoadedFont {
  LoadedFont {
    id: None,
    data: Arc::new(DEJAVU_SANS.to_vec()),
    index: 0,
    family: "DejaVu Sans".to_string(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
    face_settings: Default::default(),
  }
}

#[test]
fn line_height_uses_os2_typographic_metrics_even_without_use_typo_metrics_bit() {
  let font = dejavu_sans();
  let metrics = font.metrics().expect("metrics for DejaVuSans-subset.ttf");

  // The fixture has OS/2 USE_TYPO_METRICS disabled but provides OS/2 sTypo* metrics that differ
  // from hhea. CSS requires using OS/2 typographic metrics when available.
  assert_eq!(metrics.ascent, 1556, "unexpected ascent: {metrics:?}");
  assert_eq!(metrics.descent, -492, "unexpected descent: {metrics:?}");
  assert_eq!(metrics.line_gap, 410, "unexpected line_gap: {metrics:?}");
  assert_eq!(metrics.line_height, 2458, "unexpected line_height: {metrics:?}");
}

