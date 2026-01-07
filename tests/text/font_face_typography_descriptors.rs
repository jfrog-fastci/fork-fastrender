use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::FontFaceRule;
use fastrender::error::{Error, FontError};
use fastrender::resource::FetchedResource;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{
  FontFeatureSetting, FontLanguageOverride, FontOpticalSizing, FontVariationSetting, FontWeight,
};
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::{FontContext, FontFetcher};
use fastrender::text::pipeline::{ShapedRun, ShapingPipeline};
use fastrender::ComputedStyle;
use rustybuzz::Language as HbLanguage;
use ttf_parser::Tag;

const FALLBACK_FONT: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");
const EMOJI_FONT: &[u8] = include_bytes!("../fixtures/fonts/FastRenderEmoji.ttf");
const TEST_VAR_FONT: &[u8] = include_bytes!("../fixtures/fonts/TestVar.ttf");
const INTER_VAR_FONT: &[u8] = include_bytes!("../fixtures/fonts/Inter-Variable.ttf");
const AMSTELVAR_ALPHA_FONT: &[u8] = include_bytes!("../fixtures/fonts/AmstelvarAlpha-VF.ttf");

#[derive(Clone)]
struct FixtureFetcher {
  responses: HashMap<String, Vec<u8>>,
}

impl FixtureFetcher {
  fn new(entries: Vec<(&str, &[u8])>) -> Self {
    let mut responses = HashMap::new();
    for (url, bytes) in entries {
      responses.insert(url.to_string(), bytes.to_vec());
    }
    Self { responses }
  }
}

impl FontFetcher for FixtureFetcher {
  fn fetch(&self, url: &str, _referrer_url: Option<&str>) -> fastrender::Result<FetchedResource> {
    let data = self.responses.get(url).cloned().ok_or_else(|| {
      Error::Font(FontError::LoadFailed {
        family: url.to_string(),
        reason: "missing fixture response".into(),
      })
    })?;
    Ok(FetchedResource::with_final_url(
      data,
      Some("font/ttf".to_string()),
      Some(url.to_string()),
    ))
  }
}

fn context_with_fetcher(fetcher: Arc<dyn FontFetcher>) -> FontContext {
  let mut db = FontDatabase::empty();
  db.load_font_data(FALLBACK_FONT.to_vec())
    .expect("fallback fixture font should load");
  FontContext::with_database_and_fetcher(Arc::new(db), fetcher)
}

fn parse_faces(css: &str) -> Vec<FontFaceRule> {
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  sheet.collect_font_face_rules(&MediaContext::default())
}

fn shape_single_run(text: &str, style: &ComputedStyle, font_ctx: &FontContext) -> ShapedRun {
  let pipeline = ShapingPipeline::new();
  let runs = pipeline.shape(text, style, font_ctx).expect("shape text");
  assert_eq!(runs.len(), 1, "expected a single shaped run");
  runs.into_iter().next().unwrap()
}

fn variation_value(run: &ShapedRun, tag: [u8; 4]) -> Option<f32> {
  let tag = Tag::from_bytes(&tag);
  run
    .variations
    .iter()
    .find(|var| var.tag == tag)
    .map(|var| var.value)
}

#[test]
fn font_feature_settings_descriptor_controls_shaping() {
  let url = "https://example.test/emoji.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, EMOJI_FONT)]));

  let ctx_default = context_with_fetcher(Arc::clone(&fetcher));
  let default_faces = parse_faces(&format!(
    "@font-face {{ font-family: EmojiDefault; src: url(\"{url}\"); }}"
  ));
  assert_eq!(default_faces.len(), 1);
  assert!(default_faces[0].font_feature_settings.is_none());
  ctx_default
    .load_web_fonts(&default_faces, None, None)
    .expect("load default emoji face");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["EmojiDefault".to_string()].into();
  let run_default = shape_single_run("🇺🇸", &style, &ctx_default);
  assert_eq!(run_default.font.family, "EmojiDefault");

  let ctx_disabled = context_with_fetcher(Arc::clone(&fetcher));
  let disabled_faces = parse_faces(&format!(
    "@font-face {{ font-family: EmojiNoCcmp; src: url(\"{url}\"); font-feature-settings: \"ccmp\" 0; }}"
  ));
  assert_eq!(disabled_faces.len(), 1);
  assert!(
    disabled_faces[0]
      .font_feature_settings
      .as_deref()
      .is_some_and(|settings| settings.iter().any(|s| s.tag == *b"ccmp" && s.value == 0)),
    "expected descriptor to parse ccmp=0"
  );
  ctx_disabled
    .load_web_fonts(&disabled_faces, None, None)
    .expect("load emoji face with ccmp disabled");

  style.font_family = vec!["EmojiNoCcmp".to_string()].into();
  let run_disabled = shape_single_run("🇺🇸", &style, &ctx_disabled);
  assert_eq!(run_disabled.font.family, "EmojiNoCcmp");

  assert!(
    run_default.glyph_count() < run_disabled.glyph_count(),
    "expected disabling ccmp via @font-face descriptor to increase glyph count (default={}, disabled={})",
    run_default.glyph_count(),
    run_disabled.glyph_count()
  );
}

#[test]
fn font_variation_settings_descriptor_overrides_matching_axes() {
  let url = "https://example.test/testvar.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, TEST_VAR_FONT)]));
  let ctx = context_with_fetcher(fetcher);

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: VarFace; src: url(\"{url}\"); font-weight: 100 900; font-variation-settings: \"wght\" 900; }}"
  ));
  assert_eq!(faces.len(), 1);
  assert!(
    faces[0]
      .font_variation_settings
      .as_deref()
      .is_some_and(|settings| settings
        .iter()
        .any(|s| s.tag == *b"wght" && s.value == 900.0)),
    "expected descriptor to parse wght=900"
  );
  ctx
    .load_web_fonts(&faces, None, None)
    .expect("load var face");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["VarFace".to_string()].into();
  style.font_weight = FontWeight::Number(100);
  let run_descriptor = shape_single_run("A", &style, &ctx);
  assert_eq!(run_descriptor.font.family, "VarFace");
  assert_eq!(
    variation_value(&run_descriptor, *b"wght").unwrap_or_default(),
    900.0,
    "descriptor axis should override font-weight-derived axis"
  );

  style.font_variation_settings = vec![FontVariationSetting {
    tag: *b"wght",
    value: 200.0,
  }]
  .into();
  let run_style = shape_single_run("A", &style, &ctx);
  assert_eq!(
    variation_value(&run_style, *b"wght").unwrap_or_default(),
    200.0,
    "style font-variation-settings should override @font-face descriptor"
  );
}

fn read_u16_be(data: &[u8], offset: usize) -> Option<u16> {
  data
    .get(offset..offset + 2)
    .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn read_i32_be(data: &[u8], offset: usize) -> Option<i32> {
  data
    .get(offset..offset + 4)
    .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_fixed(data: &[u8], offset: usize) -> Option<f32> {
  let raw = read_i32_be(data, offset)?;
  Some(raw as f32 / 65536.0)
}

fn escape_css_string(value: &str) -> String {
  value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn axis_values(font: &[u8], tag: [u8; 4]) -> Option<(f32, f32, f32)> {
  let face = ttf_parser::Face::parse(font, 0).ok()?;
  let tag = Tag::from_bytes(&tag);
  let axis = face
    .variation_axes()
    .into_iter()
    .find(|axis| axis.tag == tag)?;
  Some((axis.min_value, axis.def_value, axis.max_value))
}

fn first_named_instance_with_wght(font: &[u8]) -> Option<(String, f32)> {
  let face = ttf_parser::Face::parse(font, 0).ok()?;
  let axes: Vec<_> = face.variation_axes().into_iter().collect();
  let wght_tag = Tag::from_bytes(b"wght");
  let wght_axis_idx = axes.iter().position(|axis| axis.tag == wght_tag)?;

  let fvar = face.raw_face().table(Tag::from_bytes(b"fvar"))?;
  if fvar.len() < 16 {
    return None;
  }
  let axis_count = read_u16_be(fvar, 8)? as usize;
  let axis_size = read_u16_be(fvar, 10)? as usize;
  let instance_count = read_u16_be(fvar, 12)? as usize;
  let instance_size = read_u16_be(fvar, 14)? as usize;
  let axes_offset = read_u16_be(fvar, 4)? as usize;
  let instances_offset = axes_offset.checked_add(axis_count.checked_mul(axis_size)?)?;

  let axis_count = axis_count.min(axes.len());
  for instance_idx in 0..instance_count {
    let offset = instances_offset.checked_add(instance_idx.checked_mul(instance_size)?)?;
    let name_id = read_u16_be(fvar, offset)?; // subfamilyNameID
    let coords_offset = offset.checked_add(4)?;
    if coords_offset.checked_add(axis_count.checked_mul(4)?)? > fvar.len() {
      continue;
    }

    if wght_axis_idx >= axis_count {
      continue;
    }

    let name = face
      .names()
      .into_iter()
      .filter(|name| name.name_id == name_id)
      .filter_map(|name| name.to_string())
      .find_map(|name| {
        let trimmed = name.trim();
        if trimmed.is_empty() || !trimmed.is_ascii() {
          return None;
        }
        Some(trimmed.to_string())
      })?;

    let wght = read_fixed(fvar, coords_offset + wght_axis_idx * 4)?;
    return Some((name, wght));
  }

  None
}

#[test]
fn font_named_instance_descriptor_applies_instance_axes() {
  let (font_bytes, instance_name, instance_wght) =
    [INTER_VAR_FONT, TEST_VAR_FONT, AMSTELVAR_ALPHA_FONT]
      .into_iter()
      .find_map(|bytes| {
        first_named_instance_with_wght(bytes).map(|(name, wght)| (bytes, name, wght))
      })
      .expect("fixture variable font should provide at least one named instance with wght axis");
  let escaped_instance = escape_css_string(&instance_name);

  let url = "https://example.test/named.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, font_bytes)]));
  let ctx = context_with_fetcher(fetcher);

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: NamedFace; src: url(\"{url}\"); font-weight: 100 900; font-named-instance: \"{escaped_instance}\"; }}"
  ));
  assert_eq!(faces.len(), 1);
  assert_eq!(
    faces[0].font_named_instance.as_deref(),
    Some(instance_name.as_str())
  );
  ctx
    .load_web_fonts(&faces, None, None)
    .expect("load named instance face");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["NamedFace".to_string()].into();
  style.font_weight = FontWeight::Number(100);
  let run = shape_single_run("A", &style, &ctx);
  assert_eq!(run.font.family, "NamedFace");

  let actual = variation_value(&run, *b"wght").unwrap_or_default();
  assert!(
    (actual - instance_wght).abs() < 0.001,
    "expected named instance to set wght={instance_wght}, got {actual} (instance={instance_name})"
  );
}

#[test]
fn font_language_override_descriptor_is_per_face_and_overridden_by_style() {
  let url = "https://example.test/lang.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, TEST_VAR_FONT)]));
  let ctx = context_with_fetcher(fetcher);

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: LangFace; src: url(\"{url}\"); font-language-override: \"SRB\"; }}"
  ));
  assert_eq!(faces.len(), 1);
  assert_eq!(faces[0].font_language_override.as_deref(), Some("SRB"));
  ctx
    .load_web_fonts(&faces, None, None)
    .expect("load face with language override");

  let expected_srb = HbLanguage::from_str("SRB").expect("valid hb language");
  let expected_trk = HbLanguage::from_str("TRK").expect("valid hb language");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["LangFace".to_string()].into();

  let run_descriptor = shape_single_run("A", &style, &ctx);
  assert_eq!(run_descriptor.font.family, "LangFace");
  assert_eq!(
    run_descriptor.language,
    Some(expected_srb),
    "expected @font-face descriptor to override style language when style override is normal"
  );

  style.font_language_override = FontLanguageOverride::Override("TRK".to_string());
  let run_style = shape_single_run("A", &style, &ctx);
  assert_eq!(
    run_style.language,
    Some(expected_trk),
    "expected style font-language-override to override descriptor"
  );
}

#[test]
fn font_feature_settings_style_overrides_descriptor() {
  let url = "https://example.test/emoji.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, EMOJI_FONT)]));
  let ctx = context_with_fetcher(fetcher);

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: EmojiDefault; src: url(\"{url}\"); }}\n@font-face {{ font-family: EmojiNoCcmp; src: url(\"{url}\"); font-feature-settings: \"ccmp\" 0; }}"
  ));
  assert_eq!(faces.len(), 2);
  ctx.load_web_fonts(&faces, None, None)
    .expect("load emoji faces");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["EmojiDefault".to_string()].into();
  let run_default = shape_single_run("🇺🇸", &style, &ctx);

  style.font_family = vec!["EmojiNoCcmp".to_string()].into();
  let run_disabled = shape_single_run("🇺🇸", &style, &ctx);

  style.font_feature_settings = vec![FontFeatureSetting {
    tag: *b"ccmp",
    value: 1,
  }]
  .into();
  let run_overridden = shape_single_run("🇺🇸", &style, &ctx);

  assert!(
    run_overridden.glyph_count() < run_disabled.glyph_count(),
    "expected style font-feature-settings to override descriptor (disabled={}, overridden={})",
    run_disabled.glyph_count(),
    run_overridden.glyph_count()
  );
  assert_eq!(
    run_overridden.glyph_count(),
    run_default.glyph_count(),
    "expected style override to match default shaping"
  );
}

#[test]
fn font_variation_settings_descriptor_overrides_named_instance_axes() {
  let (font_bytes, instance_name, instance_wght) =
    [INTER_VAR_FONT, TEST_VAR_FONT, AMSTELVAR_ALPHA_FONT]
      .into_iter()
      .find_map(|bytes| {
        first_named_instance_with_wght(bytes).map(|(name, wght)| (bytes, name, wght))
      })
      .expect("fixture variable font should provide a named instance with wght axis");
  let escaped_instance = escape_css_string(&instance_name);

  let (min_wght, def_wght, max_wght) =
    axis_values(font_bytes, *b"wght").expect("fixture font should expose wght axis bounds");
  let mut override_wght = max_wght;
  if (override_wght - instance_wght).abs() < 0.001 {
    override_wght = min_wght;
  }
  if (override_wght - instance_wght).abs() < 0.001 {
    override_wght = def_wght;
  }
  assert!(
    (override_wght - instance_wght).abs() >= 0.001,
    "expected override weight to differ from instance (instance={instance_wght}, override={override_wght})"
  );

  let url = "https://example.test/named_override.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, font_bytes)]));
  let ctx = context_with_fetcher(fetcher);

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: NamedOverride; src: url(\"{url}\"); font-weight: 100 900; font-named-instance: \"{escaped_instance}\"; font-variation-settings: \"wght\" {override_wght}; }}"
  ));
  assert_eq!(faces.len(), 1);
  assert_eq!(
    faces[0].font_named_instance.as_deref(),
    Some(instance_name.as_str())
  );
  assert!(
    faces[0]
      .font_variation_settings
      .as_deref()
      .is_some_and(|settings| settings.iter().any(|setting| {
        setting.tag == *b"wght" && (setting.value - override_wght).abs() < 0.001
      })),
    "expected descriptor to parse wght={override_wght}"
  );
  ctx
    .load_web_fonts(&faces, None, None)
    .expect("load named instance face with variation override");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["NamedOverride".to_string()].into();
  style.font_weight = FontWeight::Number(100);
  let run = shape_single_run("A", &style, &ctx);

  let actual = variation_value(&run, *b"wght").unwrap_or_default();
  assert!(
    (actual - override_wght).abs() < 0.001,
    "expected font-variation-settings descriptor to override named instance wght={instance_wght} with {override_wght}, got {actual}"
  );
}

#[test]
fn font_optical_sizing_auto_overrides_font_face_opsz_descriptor() {
  let url = "https://example.test/opsz.ttf";
  let fetcher: Arc<dyn FontFetcher> = Arc::new(FixtureFetcher::new(vec![(url, AMSTELVAR_ALPHA_FONT)]));
  let ctx = context_with_fetcher(fetcher);

  let (min_opsz, def_opsz, max_opsz) =
    axis_values(AMSTELVAR_ALPHA_FONT, *b"opsz").expect("fixture font should expose opsz axis");
  let descriptor_opsz = min_opsz;

  let mut font_size = (min_opsz + max_opsz) * 0.5;
  if (font_size - descriptor_opsz).abs() < 1.0 {
    font_size = (descriptor_opsz + 1.0).min(max_opsz);
  }
  if (font_size - descriptor_opsz).abs() < 0.001 {
    font_size = def_opsz;
  }
  assert!(
    (font_size - descriptor_opsz).abs() >= 0.001,
    "expected chosen font size to differ from descriptor opsz (descriptor={descriptor_opsz}, font_size={font_size})"
  );

  let faces = parse_faces(&format!(
    "@font-face {{ font-family: OpszFace; src: url(\"{url}\"); font-variation-settings: \"opsz\" {descriptor_opsz}; }}"
  ));
  assert_eq!(faces.len(), 1);
  ctx.load_web_fonts(&faces, None, None)
    .expect("load opsz face");

  let mut style = ComputedStyle::default();
  style.font_family = vec!["OpszFace".to_string()].into();
  style.font_size = font_size;

  style.font_optical_sizing = FontOpticalSizing::Auto;
  let run_auto = shape_single_run("A", &style, &ctx);
  let actual_auto = variation_value(&run_auto, *b"opsz").unwrap_or_default();
  assert!(
    (actual_auto - font_size).abs() < 0.001,
    "expected font-optical-sizing:auto to override descriptor opsz={descriptor_opsz} with font size {font_size}, got {actual_auto}"
  );

  style.font_optical_sizing = FontOpticalSizing::None;
  let run_none = shape_single_run("A", &style, &ctx);
  let actual_none = variation_value(&run_none, *b"opsz").unwrap_or_default();
  assert!(
    (actual_none - descriptor_opsz).abs() < 0.001,
    "expected font-optical-sizing:none to preserve descriptor opsz={descriptor_opsz}, got {actual_none}"
  );

  let mut override_opsz = max_opsz;
  if (override_opsz - font_size).abs() < 0.001 {
    override_opsz = min_opsz;
  }
  if (override_opsz - font_size).abs() < 0.001 {
    override_opsz = def_opsz;
  }
  assert!(
    (override_opsz - font_size).abs() >= 0.001,
    "expected authored opsz to differ from font size (font_size={font_size}, authored={override_opsz})"
  );

  style.font_optical_sizing = FontOpticalSizing::Auto;
  style.font_variation_settings = vec![FontVariationSetting {
    tag: *b"opsz",
    value: override_opsz,
  }]
  .into();
  let run_authored = shape_single_run("A", &style, &ctx);
  let actual_authored = variation_value(&run_authored, *b"opsz").unwrap_or_default();
  assert!(
    (actual_authored - override_opsz).abs() < 0.001,
    "expected style font-variation-settings to override font-optical-sizing:auto opsz={font_size} with {override_opsz}, got {actual_authored}"
  );
}
