use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use std::sync::Arc;

const DEJAVU_SANS: &[u8] = include_bytes!(concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/tests/fixtures/fonts/DejaVuSans-subset.ttf"
));

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

fn dejavu_sans_use_typo_metrics_enabled() -> LoadedFont {
  // DejaVuSans-subset.ttf ships with OS/2 sTypo* metrics that differ from hhea but has the
  // USE_TYPO_METRICS bit unset. Patch the OS/2 table to flip the bit so we can regression-test
  // the metric selection logic without adding new binary font fixtures.
  //
  // This intentionally does not update table checksums (ttf-parser and common engines do not
  // enforce them for layout), keeping the patch minimal.
  let mut data = DEJAVU_SANS.to_vec();

  // TrueType offset table:
  // - u16 numTables at offset 4
  // - table records start at offset 12 (16 bytes each)
  fn read_be_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
  }
  fn read_be_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
      data[offset],
      data[offset + 1],
      data[offset + 2],
      data[offset + 3],
    ])
  }
  fn write_be_u16(data: &mut [u8], offset: usize, value: u16) {
    let bytes = value.to_be_bytes();
    data[offset] = bytes[0];
    data[offset + 1] = bytes[1];
  }

  let num_tables = read_be_u16(&data, 4) as usize;
  let mut os2_offset = None;
  for i in 0..num_tables {
    let record = 12 + i * 16;
    let tag = &data[record..record + 4];
    if tag == b"OS/2" {
      let offset = read_be_u32(&data, record + 8) as usize;
      os2_offset = Some(offset);
      break;
    }
  }
  let os2_offset = os2_offset.expect("expected OS/2 table in DejaVuSans-subset.ttf");
  let fs_selection_offset = os2_offset + 62;
  assert!(
    fs_selection_offset + 2 <= data.len(),
    "OS/2 table too small for fsSelection field"
  );
  let fs_selection = read_be_u16(&data, fs_selection_offset);
  const USE_TYPO_METRICS: u16 = 1 << 7;
  write_be_u16(
    &mut data,
    fs_selection_offset,
    fs_selection | USE_TYPO_METRICS,
  );

  LoadedFont {
    data: Arc::new(data),
    ..dejavu_sans()
  }
}

#[test]
fn line_height_prefers_hhea_metrics_when_use_typo_metrics_bit_is_unset() {
  let font = dejavu_sans();
  let metrics = font.metrics().expect("metrics for DejaVuSans-subset.ttf");

  // The fixture has OS/2 USE_TYPO_METRICS disabled but provides OS/2 sTypo* metrics that differ
  // from hhea. In practice, browsers/FreeType prefer hhea metrics unless USE_TYPO_METRICS is set.
  assert_eq!(metrics.ascent, 1901, "unexpected ascent: {metrics:?}");
  assert_eq!(metrics.descent, -483, "unexpected descent: {metrics:?}");
  assert_eq!(metrics.line_gap, 0, "unexpected line_gap: {metrics:?}");
  assert_eq!(
    metrics.line_height, 2384,
    "unexpected line_height: {metrics:?}"
  );
}

#[test]
fn line_height_prefers_os2_metrics_when_use_typo_metrics_bit_is_set() {
  let font = dejavu_sans_use_typo_metrics_enabled();
  let metrics = font
    .metrics()
    .expect("metrics for DejaVuSans-subset.ttf with USE_TYPO_METRICS enabled");

  // With USE_TYPO_METRICS enabled, OS/2 sTypo* metrics should be selected.
  assert_eq!(metrics.ascent, 1556, "unexpected ascent: {metrics:?}");
  assert_eq!(metrics.descent, -492, "unexpected descent: {metrics:?}");
  assert_eq!(metrics.line_gap, 410, "unexpected line_gap: {metrics:?}");
  assert_eq!(
    metrics.line_height, 2458,
    "unexpected line_height: {metrics:?}"
  );
}
