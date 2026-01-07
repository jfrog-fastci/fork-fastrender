use fastrender::style::types::FontSizeAdjust;
use fastrender::style::types::FontSizeAdjustMetric;
use fastrender::text::font_db::compute_font_size_adjusted_size;
use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use std::sync::Arc;

const MVAR_METRICS_FONT: &[u8] = include_bytes!("../fixtures/fonts/mvar-metrics-test.ttf");
const NOTO_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSans-subset.ttf");
const NOTO_SANS_MONO_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSansMono-subset.ttf");
const NOTO_SANS_JP_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSansJP-subset.ttf");

fn loaded_font(data: &[u8], family: &str) -> LoadedFont {
  LoadedFont {
    id: None,
    data: Arc::new(data.to_vec()),
    index: 0,
    family: family.to_string(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
  }
}

fn assert_approx(actual: f32, expected: f32) {
  let epsilon = 1e-4;
  assert!(
    (actual - expected).abs() <= epsilon,
    "expected {expected}, got {actual}"
  );
}

#[test]
fn font_size_adjust_metric_selection_changes_used_size() {
  let font = loaded_font(MVAR_METRICS_FONT, "MVAR Metrics Test");

  let base_size = 20.0;
  let desired_ratio = 0.5;

  let ex_ratio = font
    .font_size_adjust_metric_ratio(FontSizeAdjustMetric::ExHeight)
    .expect("fixture provides x-height");
  let cap_ratio = font
    .font_size_adjust_metric_ratio(FontSizeAdjustMetric::CapHeight)
    .expect("fixture provides cap-height");
  assert!(ex_ratio != cap_ratio, "fixture metrics should differ");

  let used_ex = compute_font_size_adjusted_size(
    base_size,
    FontSizeAdjust::Number {
      ratio: desired_ratio,
      metric: FontSizeAdjustMetric::ExHeight,
    },
    &font,
    None,
  );
  let used_cap = compute_font_size_adjusted_size(
    base_size,
    FontSizeAdjust::Number {
      ratio: desired_ratio,
      metric: FontSizeAdjustMetric::CapHeight,
    },
    &font,
    None,
  );

  assert!(
    (used_ex - used_cap).abs() > 1e-3,
    "expected metric selection to affect used size (ex={used_ex}, cap={used_cap})"
  );
  assert_approx(used_ex, base_size * (desired_ratio / ex_ratio));
  assert_approx(used_cap, base_size * (desired_ratio / cap_ratio));
}

#[test]
fn font_size_adjust_ch_width_uses_zero_advance() {
  let font = loaded_font(NOTO_SANS_FONT, "Noto Sans");
  let face = ttf_parser::Face::parse(NOTO_SANS_FONT, 0).expect("parse fixture font");
  let units_per_em = face.units_per_em() as f32;
  assert!(units_per_em > 0.0);

  let glyph_id = face.glyph_index('0').expect("fixture includes '0'");
  assert!(glyph_id.0 != 0);
  let advance = face
    .glyph_hor_advance(glyph_id)
    .expect("fixture provides horizontal advance for '0'");
  let expected_ratio = advance as f32 / units_per_em;

  let ratio = font
    .font_size_adjust_metric_ratio(FontSizeAdjustMetric::ChWidth)
    .expect("metric ratio computed");
  assert_approx(ratio, expected_ratio);

  let base_size = 16.0;
  let desired_ratio = expected_ratio * 1.5;
  let used_size = compute_font_size_adjusted_size(
    base_size,
    FontSizeAdjust::Number {
      ratio: desired_ratio,
      metric: FontSizeAdjustMetric::ChWidth,
    },
    &font,
    None,
  );
  assert_approx(used_size, base_size * 1.5);
}

#[test]
fn font_size_adjust_ic_metrics_use_ideograph_advances() {
  const IDEOGRAPH: char = '\u{6C34}'; // U+6C34 '水'

  let font = loaded_font(NOTO_SANS_JP_FONT, "Noto Sans JP");
  let face = ttf_parser::Face::parse(NOTO_SANS_JP_FONT, 0).expect("parse fixture font");
  let units_per_em = face.units_per_em() as f32;
  assert!(units_per_em > 0.0);

  let glyph_id = face.glyph_index(IDEOGRAPH).expect("fixture includes ideograph");
  assert!(glyph_id.0 != 0);
  let hor = face
    .glyph_hor_advance(glyph_id)
    .expect("fixture provides horizontal advance");
  let expected_width = hor as f32 / units_per_em;
  let ver = face.glyph_ver_advance(glyph_id);
  let expected_height = ver.unwrap_or(hor) as f32 / units_per_em;

  let width_ratio = font
    .font_size_adjust_metric_ratio(FontSizeAdjustMetric::IcWidth)
    .expect("ic-width ratio");
  let height_ratio = font
    .font_size_adjust_metric_ratio(FontSizeAdjustMetric::IcHeight)
    .expect("ic-height ratio");

  assert!(width_ratio > 0.0 && width_ratio.is_finite());
  assert!(height_ratio > 0.0 && height_ratio.is_finite());

  assert_approx(width_ratio, expected_width);
  assert_approx(height_ratio, expected_height);
}

#[test]
fn font_size_adjust_ic_metrics_fallback_is_deterministic() {
  let font = loaded_font(NOTO_SANS_MONO_FONT, "Noto Sans Mono");

  // The subset Latin fixture should not contain the representative ideograph used by `ic-*`.
  assert!(
    font
      .font_size_adjust_metric_ratio(FontSizeAdjustMetric::IcWidth)
      .is_none(),
    "expected fixture to omit the ideograph used by ic-width"
  );
  assert_eq!(
    font.font_size_adjust_metric_ratio_or_fallback(FontSizeAdjustMetric::IcWidth),
    1.0
  );
}

