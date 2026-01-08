use std::collections::HashMap;
use std::sync::Arc;

use fastrender::style::types::FontFeatureSetting;
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::{assign_fonts, Direction, ItemizedRun, Script, ShapingPipeline};
use fastrender::ComputedStyle;

const FALLBACK_FONT: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");

fn features_map(run: &fastrender::text::pipeline::FontRun) -> HashMap<[u8; 4], u32> {
  let mut out = HashMap::new();
  for feature in run.features.iter() {
    out.insert(feature.tag.to_bytes(), feature.value);
  }
  out
}

#[test]
fn letter_spacing_disables_optional_ligature_features() {
  let mut db = FontDatabase::empty();
  db
    .load_font_data(FALLBACK_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let text = "fi";
  let itemized = ItemizedRun {
    start: 0,
    end: text.len(),
    text: text.to_string(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };

  let mut baseline = ComputedStyle::default();
  baseline.font_family = vec!["DejaVu Sans".to_string()].into();
  let baseline_runs = assign_fonts(&[itemized.clone()], &baseline, &ctx).expect("assign fonts");
  assert_eq!(baseline_runs.len(), 1);
  let baseline_features = features_map(&baseline_runs[0]);
  assert_eq!(baseline_features.get(b"liga"), Some(&1));
  assert_eq!(baseline_features.get(b"clig"), Some(&1));
  assert_eq!(baseline_features.get(b"dlig"), Some(&0));
  assert_eq!(baseline_features.get(b"hlig"), Some(&0));

  let mut spaced = baseline.clone();
  spaced.letter_spacing = 1.0;
  let spaced_runs = assign_fonts(&[itemized.clone()], &spaced, &ctx).expect("assign fonts");
  assert_eq!(spaced_runs.len(), 1);
  let spaced_features = features_map(&spaced_runs[0]);
  assert_eq!(spaced_features.get(b"liga"), Some(&0));
  assert_eq!(spaced_features.get(b"clig"), Some(&0));
  assert_eq!(spaced_features.get(b"dlig"), Some(&0));
  assert_eq!(spaced_features.get(b"hlig"), Some(&0));

  let mut override_style = spaced.clone();
  override_style.font_feature_settings = vec![FontFeatureSetting {
    tag: *b"liga",
    value: 1,
  }]
  .into();
  let override_runs = assign_fonts(&[itemized], &override_style, &ctx).expect("assign fonts");
  assert_eq!(override_runs.len(), 1);
  let override_features = features_map(&override_runs[0]);
  assert_eq!(override_features.get(b"liga"), Some(&1));
}

#[test]
fn shaping_cache_key_includes_letter_spacing() {
  let mut db = FontDatabase::empty();
  db
    .load_font_data(FALLBACK_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let pipeline = ShapingPipeline::new();
  assert_eq!(pipeline.cache_len(), 0);

  let mut baseline = ComputedStyle::default();
  baseline.font_family = vec!["DejaVu Sans".to_string()].into();
  pipeline
    .shape("cache me", &baseline, &ctx)
    .expect("shape baseline");
  assert_eq!(pipeline.cache_len(), 1);

  let mut spaced = baseline.clone();
  spaced.letter_spacing = 1.0;
  pipeline
    .shape("cache me", &spaced, &ctx)
    .expect("shape spaced");
  assert_eq!(
    pipeline.cache_len(),
    2,
    "letter-spacing should produce a distinct shaping cache entry"
  );
}

