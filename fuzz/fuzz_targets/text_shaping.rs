#![no_main]

use arbitrary::Unstructured;
use fastrender::style::types::{
  Direction as CssDirection, FontFeatureSetting, FontKerning, FontStretch as CssFontStretch,
  FontStyle as CssFontStyle, FontVariant, FontVariantCaps, FontVariantEmoji, FontVariantLigatures,
  FontVariantNumeric, FontVariantPosition, FontVariationSetting, NumericFigure, NumericFraction,
  NumericSpacing, TextOrientation, UnicodeBidi, WritingMode,
};
use fastrender::text::pipeline::{Direction as TextDirection, ExplicitBidiContext};
use fastrender::{ComputedStyle, FontConfig, FontContext, ShapingPipeline};
use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;
use std::sync::Once;
use unicode_bidi::Level;

const MAX_INPUT_LEN: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_FONT_FAMILY_COUNT: usize = 4;
const MAX_FONT_FAMILY_BYTES: usize = 64;
const MAX_FEATURE_SETTINGS: usize = 16;
const MAX_VARIATION_SETTINGS: usize = 8;

const MIN_FONT_SIZE: f32 = 1.0;
const MAX_FONT_SIZE: f32 = 256.0;
const MIN_SPACING: f32 = -128.0;
const MAX_SPACING: f32 = 128.0;
const MIN_OBLIQUE_ANGLE_DEG: f32 = -90.0;
const MAX_OBLIQUE_ANGLE_DEG: f32 = 90.0;

const SHAPING_CACHE_CAPACITY: &str = "1024";
const FALLBACK_CACHE_CAPACITY: &str = "4096";

static INIT: Once = Once::new();

struct FuzzTextState {
  pipeline: ShapingPipeline,
  font_context: FontContext,
}

thread_local! {
  static STATE: RefCell<Option<FuzzTextState>> = RefCell::new(None);
}

fn init_env() {
  INIT.call_once(|| {
    // Keep long-running fuzz sessions from accumulating massive caches across iterations.
    std::env::set_var("FASTR_TEXT_SHAPING_CACHE_CAPACITY", SHAPING_CACHE_CAPACITY);
    std::env::set_var("FASTR_TEXT_FALLBACK_CACHE_CAPACITY", FALLBACK_CACHE_CAPACITY);
  });
}

fn bounded_f32(raw: f32, min: f32, max: f32, default: f32) -> f32 {
  if raw.is_finite() {
    raw.clamp(min, max)
  } else {
    default
  }
}

fn bounded_font_size(raw: f32) -> f32 {
  bounded_f32(raw.abs(), MIN_FONT_SIZE, MAX_FONT_SIZE, 16.0)
}

fn bounded_spacing(raw: f32) -> f32 {
  bounded_f32(raw, MIN_SPACING, MAX_SPACING, 0.0)
}

fn bounded_oblique_angle(raw: f32) -> f32 {
  bounded_f32(raw, MIN_OBLIQUE_ANGLE_DEG, MAX_OBLIQUE_ANGLE_DEG, 0.0)
}

fn truncate_string_to_bytes(mut value: String, max_bytes: usize) -> String {
  if value.len() <= max_bytes {
    return value;
  }
  let mut end = max_bytes;
  while end > 0 && !value.is_char_boundary(end) {
    end -= 1;
  }
  value.truncate(end);
  value
}

fn choose<T: Copy>(unstructured: &mut Unstructured, candidates: &[T], fallback: T) -> T {
  match unstructured.choose(candidates) {
    Ok(value) => *value,
    Err(_) => fallback,
  }
}

fn build_font_weight(unstructured: &mut Unstructured) -> fastrender::style::types::FontWeight {
  use fastrender::style::types::FontWeight as CssFontWeight;
  match unstructured.int_in_range::<u8>(0..=4).unwrap_or(0) {
    0 => CssFontWeight::Normal,
    1 => CssFontWeight::Bold,
    2 => CssFontWeight::Bolder,
    3 => CssFontWeight::Lighter,
    _ => CssFontWeight::Number(
      unstructured
        .arbitrary::<u16>()
        .unwrap_or(400)
        .clamp(1, 1000),
    ),
  }
}

fn build_font_style(unstructured: &mut Unstructured) -> CssFontStyle {
  match unstructured.int_in_range::<u8>(0..=2).unwrap_or(0) {
    0 => CssFontStyle::Normal,
    1 => CssFontStyle::Italic,
    _ => {
      let has_angle = unstructured.arbitrary::<bool>().unwrap_or(false);
      let angle = has_angle
        .then(|| bounded_oblique_angle(unstructured.arbitrary::<f32>().unwrap_or(0.0)));
      CssFontStyle::Oblique(angle)
    }
  }
}

fn build_font_stretch(unstructured: &mut Unstructured) -> CssFontStretch {
  match unstructured.int_in_range::<u8>(0..=9).unwrap_or(4) {
    0 => CssFontStretch::UltraCondensed,
    1 => CssFontStretch::ExtraCondensed,
    2 => CssFontStretch::Condensed,
    3 => CssFontStretch::SemiCondensed,
    4 => CssFontStretch::Normal,
    5 => CssFontStretch::SemiExpanded,
    6 => CssFontStretch::Expanded,
    7 => CssFontStretch::ExtraExpanded,
    8 => CssFontStretch::UltraExpanded,
    _ => CssFontStretch::from_percentage(
      bounded_f32(
        unstructured.arbitrary::<f32>().unwrap_or(100.0),
        50.0,
        200.0,
        100.0,
      ),
    ),
  }
}

fn build_font_families(unstructured: &mut Unstructured) -> std::sync::Arc<[String]> {
  const KNOWN_FAMILIES: &[&str] = &[
    // Generic families.
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    // FastRender bundled fonts / special generics.
    "emoji",
    "math",
    "DejaVu Sans",
    "STIX Two Math",
    "FastRender Emoji",
  ];

  let requested = unstructured
    .int_in_range::<u8>(1..=MAX_FONT_FAMILY_COUNT as u8)
    .unwrap_or(1) as usize;

  // Always include a generic family so font resolution is deterministic even if the
  // randomized names don't match anything in the bundled font DB.
  let mut families = Vec::with_capacity(requested);
  families.push("serif".to_string());

  for _ in 1..requested {
    let use_known = unstructured.arbitrary::<bool>().unwrap_or(true);
    if use_known {
      families.push(choose(unstructured, KNOWN_FAMILIES, "serif").to_string());
      continue;
    }

    let len = unstructured
      .int_in_range::<u8>(0..=MAX_FONT_FAMILY_BYTES as u8)
      .unwrap_or(0) as usize;
    if let Ok(bytes) = unstructured.bytes(len) {
      let candidate = truncate_string_to_bytes(String::from_utf8_lossy(bytes).into_owned(), MAX_FONT_FAMILY_BYTES);
      if !candidate.is_empty() {
        families.push(candidate);
      }
    }
  }

  families.into()
}

fn build_feature_settings(unstructured: &mut Unstructured) -> std::sync::Arc<[FontFeatureSetting]> {
  let count = unstructured
    .int_in_range::<u8>(0..=MAX_FEATURE_SETTINGS as u8)
    .unwrap_or(0) as usize;
  let mut settings = Vec::with_capacity(count);
  for _ in 0..count {
    let tag = unstructured.arbitrary::<[u8; 4]>().unwrap_or(*b"liga");
    let raw_value = unstructured.arbitrary::<u32>().unwrap_or(0);
    // Keep values small to avoid weird interactions in downstream OpenType code paths.
    let value = raw_value.min(1024);
    settings.push(FontFeatureSetting { tag, value });
  }
  settings.into()
}

fn build_variation_settings(
  unstructured: &mut Unstructured,
) -> std::sync::Arc<[FontVariationSetting]> {
  let count = unstructured
    .int_in_range::<u8>(0..=MAX_VARIATION_SETTINGS as u8)
    .unwrap_or(0) as usize;
  let mut settings = Vec::with_capacity(count);
  for _ in 0..count {
    let tag = unstructured.arbitrary::<[u8; 4]>().unwrap_or(*b"wght");
    let raw_value = unstructured.arbitrary::<f32>().unwrap_or(0.0);
    let value = bounded_f32(raw_value, -1000.0, 1000.0, 0.0);
    settings.push(FontVariationSetting { tag, value });
  }
  settings.into()
}

fn build_style(unstructured: &mut Unstructured) -> ComputedStyle {
  let mut style = ComputedStyle::default();

  style.direction = choose(
    unstructured,
    &[CssDirection::Ltr, CssDirection::Rtl],
    CssDirection::Ltr,
  );
  style.unicode_bidi = choose(
    unstructured,
    &[
      UnicodeBidi::Normal,
      UnicodeBidi::Embed,
      UnicodeBidi::BidiOverride,
      UnicodeBidi::Isolate,
      UnicodeBidi::IsolateOverride,
      UnicodeBidi::Plaintext,
    ],
    UnicodeBidi::Normal,
  );
  style.writing_mode = choose(
    unstructured,
    &[
      WritingMode::HorizontalTb,
      WritingMode::VerticalRl,
      WritingMode::VerticalLr,
      WritingMode::SidewaysRl,
      WritingMode::SidewaysLr,
    ],
    WritingMode::HorizontalTb,
  );
  style.text_orientation = choose(
    unstructured,
    &[
      TextOrientation::Mixed,
      TextOrientation::Upright,
      TextOrientation::Sideways,
      TextOrientation::SidewaysLeft,
      TextOrientation::SidewaysRight,
    ],
    TextOrientation::Mixed,
  );

  style.font_size = bounded_font_size(unstructured.arbitrary::<f32>().unwrap_or(style.font_size));
  style.font_weight = build_font_weight(unstructured);
  style.font_style = build_font_style(unstructured);
  style.font_stretch = build_font_stretch(unstructured);
  style.letter_spacing = bounded_spacing(unstructured.arbitrary::<f32>().unwrap_or(0.0));
  style.word_spacing = bounded_spacing(unstructured.arbitrary::<f32>().unwrap_or(0.0));
  style.font_kerning = choose(
    unstructured,
    &[FontKerning::Auto, FontKerning::Normal, FontKerning::None],
    FontKerning::Auto,
  );

  style.font_variant = choose(
    unstructured,
    &[FontVariant::Normal, FontVariant::SmallCaps],
    FontVariant::Normal,
  );
  style.font_variant_caps = choose(
    unstructured,
    &[
      FontVariantCaps::Normal,
      FontVariantCaps::SmallCaps,
      FontVariantCaps::AllSmallCaps,
      FontVariantCaps::PetiteCaps,
      FontVariantCaps::AllPetiteCaps,
      FontVariantCaps::Unicase,
      FontVariantCaps::TitlingCaps,
    ],
    FontVariantCaps::Normal,
  );
  style.font_variant_position = choose(
    unstructured,
    &[
      FontVariantPosition::Normal,
      FontVariantPosition::Sub,
      FontVariantPosition::Super,
    ],
    FontVariantPosition::Normal,
  );
  style.font_variant_emoji = choose(
    unstructured,
    &[
      FontVariantEmoji::Normal,
      FontVariantEmoji::Emoji,
      FontVariantEmoji::Text,
      FontVariantEmoji::Unicode,
    ],
    FontVariantEmoji::Normal,
  );

  style.font_variant_ligatures = FontVariantLigatures {
    common: unstructured.arbitrary::<bool>().unwrap_or(true),
    discretionary: unstructured.arbitrary::<bool>().unwrap_or(false),
    historical: unstructured.arbitrary::<bool>().unwrap_or(false),
    contextual: unstructured.arbitrary::<bool>().unwrap_or(true),
  };

  style.font_variant_numeric = FontVariantNumeric {
    figure: choose(
      unstructured,
      &[NumericFigure::Normal, NumericFigure::Lining, NumericFigure::Oldstyle],
      NumericFigure::Normal,
    ),
    spacing: choose(
      unstructured,
      &[
        NumericSpacing::Normal,
        NumericSpacing::Proportional,
        NumericSpacing::Tabular,
      ],
      NumericSpacing::Normal,
    ),
    fraction: choose(
      unstructured,
      &[NumericFraction::Normal, NumericFraction::Diagonal, NumericFraction::Stacked],
      NumericFraction::Normal,
    ),
    ordinal: unstructured.arbitrary::<bool>().unwrap_or(false),
    slashed_zero: unstructured.arbitrary::<bool>().unwrap_or(false),
  };

  style.font_family = build_font_families(unstructured);
  style.font_feature_settings = build_feature_settings(unstructured);
  style.font_variation_settings = build_variation_settings(unstructured);

  // Keep language tags small, but vary them enough to exercise script fallback code paths.
  const LANGS: &[&str] = &[
    "en",
    "en-US",
    "ar",
    "he",
    "fa",
    "ru",
    "zh-Hans",
    "zh-Hant",
    "ja",
    "ko",
    "th",
    "lo",
    "km",
    "my",
  ];
  style.language = choose(unstructured, LANGS, "en").into();

  style
}

fn build_explicit_bidi_context(unstructured: &mut Unstructured) -> Option<ExplicitBidiContext> {
  if !unstructured.arbitrary::<bool>().unwrap_or(false) {
    return None;
  }
  let raw_level = unstructured.int_in_range::<u8>(0..=8).unwrap_or(0);
  let fallback = if raw_level % 2 == 0 {
    Level::ltr()
  } else {
    Level::rtl()
  };
  let level = Level::new(raw_level).unwrap_or(fallback);
  let override_all = unstructured.arbitrary::<bool>().unwrap_or(false);
  Some(ExplicitBidiContext { level, override_all })
}

fn build_text(data: &[u8]) -> String {
  let source = if data.len() > MAX_TEXT_BYTES {
    &data[..MAX_TEXT_BYTES]
  } else {
    data
  };
  let text = String::from_utf8_lossy(source).into_owned();
  truncate_string_to_bytes(text, MAX_TEXT_BYTES)
}

fuzz_target!(|data: &[u8]| {
  init_env();

  let bytes = if data.len() > MAX_INPUT_LEN {
    &data[..MAX_INPUT_LEN]
  } else {
    data
  };
  let mut unstructured = Unstructured::new(bytes);

  let style = build_style(&mut unstructured);
  let explicit_bidi = build_explicit_bidi_context(&mut unstructured);
  let base_direction = if unstructured.arbitrary::<bool>().unwrap_or(false) {
    TextDirection::RightToLeft
  } else {
    TextDirection::LeftToRight
  };
  let text = build_text(bytes);

  STATE.with(|cell| {
    if cell.borrow().is_none() {
      let pipeline = ShapingPipeline::new();
      let font_context = FontContext::with_config(FontConfig::bundled_only());
      *cell.borrow_mut() = Some(FuzzTextState {
        pipeline,
        font_context,
      });
    }

    let guard = cell.borrow_mut();
    let Some(state) = guard.as_ref() else {
      return;
    };

    let _ = state.pipeline.shape(&text, &style, &state.font_context);
    let _ =
      state
        .pipeline
        .shape_with_context(&text, &style, &state.font_context, base_direction, explicit_bidi);
  });
});
