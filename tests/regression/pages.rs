use crate::r#ref;

use chrono::Utc;
use fastrender::api::DiagnosticsLevel;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::style::media::MediaType;
use fastrender::{FastRender, FontConfig, RenderOptions, RenderStageTimings, ResourcePolicy};
use r#ref::image_compare::{compare_config_from_env, save_artifacts, CompareEnvVars};
use r#ref::{compare_images, load_png_from_bytes, CompareConfig};
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::Instant;
use url::Url;

#[derive(Clone, Copy)]
struct PageShot {
  label: &'static str,
  viewport: (u32, u32),
  dpr: f32,
  media: MediaType,
  force_parallel_tiling: bool,
}

impl PageShot {
  fn golden_name(&self, page: &str) -> String {
    if self.label == "default" {
      page.to_string()
    } else {
      format!("{}_{}", page, self.label)
    }
  }
}

struct PageFixture {
  name: &'static str,
  html: &'static str,
  shots: &'static [PageShot],
}

const DEFAULT_SHOT: PageShot = PageShot {
  label: "default",
  viewport: (1040, 1240),
  dpr: 1.0,
  media: MediaType::Screen,
  force_parallel_tiling: false,
};

const PRINT_SHOT: PageShot = PageShot {
  label: "print",
  viewport: (920, 1180),
  dpr: 1.0,
  media: MediaType::Print,
  force_parallel_tiling: false,
};

const DEFAULT_SHOTS: &[PageShot] = &[DEFAULT_SHOT];
const PRINT_SHOTS: &[PageShot] = &[PRINT_SHOT];
const PARALLEL_TILING_SHOTS: &[PageShot] = &[PageShot {
  label: "default",
  viewport: (1200, 800),
  dpr: 1.0,
  media: MediaType::Screen,
  // This fixture is only meaningful when the display-list renderer is split into tiles, since the
  // bug only reproduces when each tile paints into a translated canvas.
  force_parallel_tiling: true,
}];

const PAGE_FIXTURES: &[PageFixture] = &[
  PageFixture {
    name: "flex_dashboard",
    html: "flex_dashboard/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "grid_news",
    html: "grid_news/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "intrinsic_sizing_keywords",
    html: "intrinsic_sizing_keywords/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "anchor_positioning_basic",
    html: "anchor_positioning_basic/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_financial",
    html: "table_financial/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "multicol_article",
    html: "multicol_article/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "paginated_report",
    html: "paginated_report/index.html",
    shots: PRINT_SHOTS,
  },
  PageFixture {
    name: "page_break_sides_blank_page",
    html: "page_break_sides_blank_page/index.html",
    shots: PRINT_SHOTS,
  },
  PageFixture {
    name: "fragmentation_showcase",
    html: "fragmentation_showcase/index.html",
    shots: PRINT_SHOTS,
  },
  PageFixture {
    name: "mask_filter_showcase",
    html: "mask_filter_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "vendor_prefixes",
    html: "vendor_prefixes/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "fill_available_height",
    html: "fill_available_height/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "webkit_line_clamp",
    html: "webkit_line_clamp/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "mask_parallel_tiling_translation",
    html: "mask_parallel_tiling_translation/index.html",
    shots: PARALLEL_TILING_SHOTS,
  },
  PageFixture {
    name: "svg_embed",
    html: "svg_embed/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "svg_css_presentation",
    html: "svg_css_presentation/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "svg_filter_url_showcase",
    html: "svg_filter_url_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_modes",
    html: "writing_modes/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "container_query_fixpoint",
    html: "container_query_fixpoint/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_showcase",
    html: "subgrid_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_alignment",
    html: "subgrid_alignment/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_writing_mode_gap",
    html: "subgrid_writing_mode_gap/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_vertical_inheritance",
    html: "subgrid_vertical_inheritance/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_vertical_stack",
    html: "subgrid_vertical_stack/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "subgrid_nested_axes",
    html: "subgrid_nested_axes/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls",
    html: "form_controls/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "positioned_badge_regression",
    html: "positioned_badge_regression/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "positioned_containing_block_filter",
    html: "positioned_containing_block_filter/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "individual_transforms",
    html: "individual_transforms/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_heavy_document",
    html: "selector_heavy_document/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_deep_dom_has",
    html: "selector_deep_dom_has/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_incident_console",
    html: "selector_incident_console/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_has_dashboard",
    html: "selector_has_dashboard/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_cascade_matrix",
    html: "selector_cascade_matrix/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_descendant_stress",
    html: "selector_descendant_stress/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "selector_labyrinth_dashboard",
    html: "selector_labyrinth_dashboard/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_complex",
    html: "table_complex/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_colgroup_layout",
    html: "table_colgroup_layout/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_cross_tab",
    html: "table_cross_tab/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_colgroup_matrix",
    html: "table_colgroup_matrix/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_colgroup_spanning",
    html: "table_colgroup_spanning/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_span_minimal",
    html: "table_span_minimal/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_span_layout",
    html: "table_span_layout/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "table_financial_report",
    html: "table_financial_report/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_scene",
    html: "filter_backdrop_scene/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_composite_lab",
    html: "filter_composite_lab/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_masking",
    html: "filter_backdrop_masking/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_glass",
    html: "filter_backdrop_glass/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_layers",
    html: "filter_backdrop_layers/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_stagecraft",
    html: "filter_backdrop_stagecraft/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "filter_backdrop_atrium",
    html: "filter_backdrop_atrium/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "ruby_vertical_text",
    html: "ruby_vertical_text/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "vertical_ruby_composition",
    html: "vertical_ruby_composition/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_mode_vertical_ruby",
    html: "writing_mode_vertical_ruby/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_mode_ruby_combine",
    html: "writing_mode_ruby_combine/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_mode_ruby_vertical_mix",
    html: "writing_mode_ruby_vertical_mix/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_mode_vertical_story",
    html: "writing_mode_vertical_story/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "writing_mode_vertical_annotations",
    html: "writing_mode_vertical_annotations/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_appearance",
    html: "form_controls_appearance/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_range_select",
    html: "form_controls_range_select/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_showcase",
    html: "form_controls_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_states",
    html: "form_controls_states/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_custom_vs_default",
    html: "form_controls_custom_vs_default/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "mdn_switch_toggle_appearance_none",
    html: "mdn_switch_toggle_appearance_none/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_comparison_panel",
    html: "form_controls_comparison_panel/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_lab",
    html: "form_controls_lab/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_placeholder",
    html: "form_controls_placeholder/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "form_controls_placeholder_pseudo",
    html: "form_controls_placeholder_pseudo/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_scene",
    html: "preserve_3d_scene/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_stack",
    html: "preserve_3d_stack/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_cards",
    html: "preserve_3d_cards/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_layers",
    html: "preserve_3d_layers/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_outsets",
    html: "preserve_3d_outsets/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_backface_nested",
    html: "preserve_3d_backface_nested/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_blend_mode",
    html: "preserve_3d_blend_mode/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_perspective_grid",
    html: "preserve_3d_perspective_grid/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_product_showcase",
    html: "preserve_3d_product_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_showroom",
    html: "preserve_3d_showroom/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "preserve_3d_projective_clip",
    html: "preserve_3d_projective_clip/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_object_fit",
    html: "image_grid_object_fit/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_picture_sources",
    html: "image_grid_picture_sources/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_responsive_srcset",
    html: "image_grid_responsive_srcset/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_picture_masonry",
    html: "image_grid_picture_masonry/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_picture_artboard",
    html: "image_grid_picture_artboard/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_picture_object_fit",
    html: "image_grid_picture_object_fit/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "image_grid_picture_showcase",
    html: "image_grid_picture_showcase/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "replaced_max_width_probe",
    html: "replaced_max_width_probe/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "font_face_metric_overrides",
    html: "font_face_metric_overrides/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "hidden_attribute_display_none",
    html: "hidden_attribute_display_none/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "intrinsic_sizing_block_keywords",
    html: "intrinsic_sizing_block_keywords/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "logical_border_shorthands",
    html: "logical_border_shorthands/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "line_clamp",
    html: "line_clamp/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "br_linebreak",
    html: "br_linebreak/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "multiscript_font_fallback",
    html: "multiscript_font_fallback/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "background_clip_text",
    html: "background_clip_text/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "overflow_axis_interaction",
    html: "overflow_axis_interaction/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "amazon.com",
    html: "amazon.com/index.html",
    shots: DEFAULT_SHOTS,
  },
  PageFixture {
    name: "twitch.tv",
    html: "twitch.tv/index.html",
    shots: DEFAULT_SHOTS,
  },
];

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/fixtures")
}

fn golden_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/golden")
}

fn pages_overrides_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/overrides.toml")
}

fn pages_output_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/pages-output")
}

fn golden_path(name: &str) -> PathBuf {
  golden_dir().join(format!("{name}.png"))
}

fn should_update_goldens() -> bool {
  std::env::var("UPDATE_PAGES_GOLDEN").is_ok()
}

fn fixture_filter() -> Option<Vec<String>> {
  let raw = std::env::var("PAGES_FIXTURE_FILTER")
    .ok()
    .or_else(|| std::env::var("PAGES_FIXTURE").ok())?;
  let parts = raw
    .split(',')
    .map(|part| part.trim().to_string())
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>();
  (!parts.is_empty()).then_some(parts)
}

const PAGES_OVERRIDES_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
struct PagesOverrides {
  schema_version: u32,
  #[serde(default)]
  rules: Vec<PagesOverrideRule>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PagesOverrideMatch {
  Exact,
  Prefix,
}

#[derive(Debug, Deserialize)]
struct PagesOverrideRule {
  #[serde(rename = "match")]
  match_kind: PagesOverrideMatch,
  fixture: String,
  #[serde(default)]
  min_max_different_percent: Option<f64>,
  #[serde(default)]
  min_channel_tolerance: Option<u8>,
  #[serde(default)]
  ignore_alpha: Option<bool>,
  #[serde(default)]
  min_max_perceptual_distance: Option<f64>,
}

impl PagesOverrides {
  fn apply_to(&self, fixture_name: &str, config: &mut CompareConfig) {
    for rule in &self.rules {
      if rule.matches(fixture_name) {
        rule.apply(config);
      }
    }
  }
}

impl PagesOverrideRule {
  fn matches(&self, fixture_name: &str) -> bool {
    match self.match_kind {
      PagesOverrideMatch::Exact => fixture_name == self.fixture,
      PagesOverrideMatch::Prefix => fixture_name.starts_with(&self.fixture),
    }
  }

  fn apply(&self, config: &mut CompareConfig) {
    if let Some(min_percent) = self.min_max_different_percent {
      config.max_different_percent = config.max_different_percent.max(min_percent);
    }
    if let Some(min_tolerance) = self.min_channel_tolerance {
      config.channel_tolerance = config.channel_tolerance.max(min_tolerance);
    }
    if self.ignore_alpha.unwrap_or(false) {
      config.compare_alpha = false;
    }
    if let Some(min_distance) = self.min_max_perceptual_distance {
      if let Some(existing) = config.max_perceptual_distance {
        config.max_perceptual_distance = Some(existing.max(min_distance));
      }
    }
  }
}

static PAGES_OVERRIDES: OnceLock<Result<PagesOverrides, String>> = OnceLock::new();

fn load_pages_overrides() -> Result<&'static PagesOverrides, String> {
  match PAGES_OVERRIDES.get_or_init(|| {
    let path = pages_overrides_path();
    let raw = fs::read_to_string(&path)
      .map_err(|e| format!("Failed to read pages overrides {}: {e}", path.display()))?;
    let overrides: PagesOverrides = toml::from_str(&raw)
      .map_err(|e| format!("Failed to parse pages overrides {}: {e}", path.display()))?;
    if overrides.schema_version != PAGES_OVERRIDES_SCHEMA_VERSION {
      return Err(format!(
        "Unexpected pages overrides schema_version {} (expected {}) in {}",
        overrides.schema_version,
        PAGES_OVERRIDES_SCHEMA_VERSION,
        path.display()
      ));
    }
    Ok(overrides)
  }) {
    Ok(overrides) => Ok(overrides),
    Err(err) => Err(err.clone()),
  }
}

const PAGES_REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum PagesReportStatus {
  Pass,
  Fail,
  Error,
}

impl PagesReportStatus {
  fn is_failure(self) -> bool {
    matches!(self, PagesReportStatus::Fail | PagesReportStatus::Error)
  }

  fn label(self) -> &'static str {
    match self {
      PagesReportStatus::Pass => "PASS",
      PagesReportStatus::Fail => "FAIL",
      PagesReportStatus::Error => "ERROR",
    }
  }
}

#[derive(Debug, Clone, Serialize)]
struct PagesCompareConfigSummary {
  channel_tolerance: u8,
  max_different_percent: f64,
  compare_alpha: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  max_perceptual_distance: Option<f64>,
}

impl From<&CompareConfig> for PagesCompareConfigSummary {
  fn from(config: &CompareConfig) -> Self {
    Self {
      channel_tolerance: config.channel_tolerance,
      max_different_percent: config.max_different_percent,
      compare_alpha: config.compare_alpha,
      max_perceptual_distance: config.max_perceptual_distance,
    }
  }
}

#[derive(Debug, Clone, Serialize)]
struct PagesCompareMetrics {
  different_pixels: u64,
  total_pixels: u64,
  different_percent: f64,
  max_channel_diff: u8,
  perceptual_distance: f64,
}

#[derive(Debug, Clone, Serialize)]
struct PagesCompareArtifacts {
  #[serde(skip_serializing_if = "Option::is_none")]
  actual: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  expected: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  diff: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PagesReportEntry {
  fixture: String,
  shot: String,
  golden_name: String,
  status: PagesReportStatus,
  compare_config: PagesCompareConfigSummary,
  #[serde(skip_serializing_if = "Option::is_none")]
  metrics: Option<PagesCompareMetrics>,
  #[serde(skip_serializing_if = "Option::is_none")]
  artifacts: Option<PagesCompareArtifacts>,
  #[serde(skip_serializing_if = "Option::is_none")]
  message: Option<String>,
}

#[derive(Debug, Serialize)]
struct PagesReportTotals {
  total: usize,
  passed: usize,
  failed: usize,
  errors: usize,
}

#[derive(Debug, Serialize)]
struct PagesReport {
  schema_version: u32,
  generated_at: String,
  totals: PagesReportTotals,
  results: Vec<PagesReportEntry>,
}

fn pages_report_enabled() -> bool {
  std::env::var_os("PAGES_REPORT").is_some()
}

fn pages_max_failures_from_env() -> Result<usize, String> {
  if let Ok(raw) = std::env::var("PAGES_MAX_FAILURES") {
    let parsed = raw
      .parse::<usize>()
      .map_err(|e| format!("Invalid PAGES_MAX_FAILURES '{raw}': {e}"))?;
    if parsed == 0 {
      return Err("PAGES_MAX_FAILURES must be >= 1".to_string());
    }
    return Ok(parsed);
  }

  let fail_fast = std::env::var("PAGES_FAIL_FAST")
    .ok()
    .map(|v| v != "0")
    .unwrap_or(true);
  Ok(if fail_fast { 1 } else { usize::MAX })
}

fn pages_report_paths(output_dir: &Path) -> (PathBuf, PathBuf) {
  (output_dir.join("report.html"), output_dir.join("report.json"))
}

fn report_rel_path(output_dir: &Path, path: &Path) -> String {
  let rel = path.strip_prefix(output_dir).unwrap_or(path);
  rel.to_string_lossy().replace('\\', "/")
}

fn write_pages_report(output_dir: &Path, results: Vec<PagesReportEntry>) -> Result<(), String> {
  fs::create_dir_all(output_dir)
    .map_err(|e| format!("Failed to create pages output dir {}: {e}", output_dir.display()))?;

  let mut passed = 0usize;
  let mut failed = 0usize;
  let mut errors = 0usize;
  for entry in &results {
    match entry.status {
      PagesReportStatus::Pass => passed += 1,
      PagesReportStatus::Fail => failed += 1,
      PagesReportStatus::Error => errors += 1,
    }
  }

  let totals = PagesReportTotals {
    total: results.len(),
    passed,
    failed,
    errors,
  };

  let report = PagesReport {
    schema_version: PAGES_REPORT_SCHEMA_VERSION,
    generated_at: Utc::now().to_rfc3339(),
    totals,
    results,
  };

  let (html_path, json_path) = pages_report_paths(output_dir);
  let json = serde_json::to_vec_pretty(&report)
    .map_err(|e| format!("Failed to serialize pages report JSON: {e}"))?;
  fs::write(&json_path, json)
    .map_err(|e| format!("Failed to write {}: {e}", json_path.display()))?;

  let mut rows = String::new();
  for entry in &report.results {
    let diff_pct = entry.metrics.as_ref().map(|m| m.different_percent).unwrap_or(0.0);
    let max_diff = entry.metrics.as_ref().map(|m| m.max_channel_diff).unwrap_or(0);
    let perceptual = entry
      .metrics
      .as_ref()
      .map(|m| m.perceptual_distance)
      .unwrap_or(0.0);
    let message = entry
      .message
      .as_deref()
      .unwrap_or("")
      .replace('&', "&amp;")
      .replace('<', "&lt;")
      .replace('>', "&gt;");

    let links = if let Some(artifacts) = entry.artifacts.as_ref() {
      let mut parts = Vec::new();
      if let Some(expected) = artifacts.expected.as_deref() {
        parts.push(format!(
          r#"<a href="{p}">expected</a>"#,
          p = fastrender::cli_utils::report::escape_html(expected)
        ));
      }
      if let Some(actual) = artifacts.actual.as_deref() {
        parts.push(format!(
          r#"<a href="{p}">actual</a>"#,
          p = fastrender::cli_utils::report::escape_html(actual)
        ));
      }
      if let Some(diff) = artifacts.diff.as_deref() {
        parts.push(format!(
          r#"<a href="{p}">diff</a>"#,
          p = fastrender::cli_utils::report::escape_html(diff)
        ));
      }
      parts.join(" | ")
    } else {
      String::new()
    };

    let _ = writeln!(
      rows,
      "<tr data-status=\"{}\" data-diff=\"{:.4}\" data-maxdiff=\"{}\" data-perceptual=\"{:.6}\">\
         <td>{}</td>\
         <td>{}</td>\
         <td>{}</td>\
         <td>{:.4}</td>\
         <td>{}</td>\
         <td>{:.4}</td>\
         <td>{}</td>\
         <td>{}</td>\
       </tr>",
      entry.status.label(),
      diff_pct,
      max_diff,
      perceptual,
      fastrender::cli_utils::report::escape_html(&entry.fixture),
      fastrender::cli_utils::report::escape_html(&entry.shot),
      entry.status.label(),
      diff_pct,
      max_diff,
      perceptual,
      links,
      message
    );
  }

  let report_html = format!(
    r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>pages regression report</title>
  <style>
    body {{ font-family: sans-serif; margin: 16px; }}
    table {{ border-collapse: collapse; width: 100%; }}
    th, td {{ border: 1px solid #ccc; padding: 6px; font-size: 14px; }}
    thead {{ background: #f6f6f6; position: sticky; top: 0; }}
    th.sortable {{ cursor: pointer; }}
    .controls label {{ margin-right: 12px; }}
  </style>
</head>
<body>
  <h1>Offline pages regression report</h1>
  <p>Total: {total}. Passed: {passed}. Failed: {failed}. Errors: {errors}. JSON: <a href="{json_name}">{json_name}</a></p>
  <div class="controls">
    <label><input type="checkbox" name="status" value="PASS" checked>Pass</label>
    <label><input type="checkbox" name="status" value="FAIL" checked>Fail</label>
    <label><input type="checkbox" name="status" value="ERROR" checked>Error</label>
    <label style="margin-left:16px;">Min diff % <input type="number" id="min-diff" value="0" step="0.01" style="width:90px;"></label>
  </div>
  <table>
    <thead>
      <tr>
        <th class="sortable" data-sort="fixture">Fixture</th>
        <th class="sortable" data-sort="shot">Shot</th>
        <th class="sortable" data-sort="status">Status</th>
        <th class="sortable" data-sort="diff">Diff %</th>
        <th class="sortable" data-sort="maxdiff">Max Δ</th>
        <th class="sortable" data-sort="perceptual">Perceptual</th>
        <th>Links</th>
        <th>Message</th>
      </tr>
    </thead>
    <tbody id="results">
      {rows}
    </tbody>
  </table>
  <script>
    const tbody = document.getElementById('results');
    const statusWeight = (status) => {{
      switch (status) {{
        case 'ERROR': return 3;
        case 'FAIL': return 2;
        case 'PASS': return 1;
        default: return 0;
      }}
    }};
    let currentSort = {{ key: 'diff', asc: false }};

    const applyFilters = () => {{
      const statuses = Array.from(document.querySelectorAll('input[name="status"]:checked')).map(el => el.value);
      const minDiff = parseFloat(document.getElementById('min-diff').value || '0');
      Array.from(tbody.querySelectorAll('tr')).forEach(row => {{
        const status = row.dataset.status;
        const diff = parseFloat(row.dataset.diff || '0');
        row.style.display = (statuses.includes(status) && diff >= minDiff) ? '' : 'none';
      }});
    }};

    const sortRows = () => {{
      const rows = Array.from(tbody.querySelectorAll('tr'));
      const key = currentSort.key;
      const asc = currentSort.asc;
      rows.sort((a, b) => {{
        if (key === 'status') {{
          const aw = statusWeight(a.dataset.status);
          const bw = statusWeight(b.dataset.status);
          return asc ? (aw - bw) : (bw - aw);
        }}
        if (key === 'diff') {{
          const av = parseFloat(a.dataset.diff || '0');
          const bv = parseFloat(b.dataset.diff || '0');
          return asc ? (av - bv) : (bv - av);
        }}
        if (key === 'maxdiff') {{
          const av = parseInt(a.dataset.maxdiff || '0', 10);
          const bv = parseInt(b.dataset.maxdiff || '0', 10);
          return asc ? (av - bv) : (bv - av);
        }}
        if (key === 'perceptual') {{
          const av = parseFloat(a.dataset.perceptual || '0');
          const bv = parseFloat(b.dataset.perceptual || '0');
          return asc ? (av - bv) : (bv - av);
        }}
        const aText = a.children[key === 'fixture' ? 0 : 1].textContent || '';
        const bText = b.children[key === 'fixture' ? 0 : 1].textContent || '';
        return asc ? aText.localeCompare(bText) : bText.localeCompare(aText);
      }});
      rows.forEach(row => tbody.appendChild(row));
    }};

    document.querySelectorAll('input[name="status"]').forEach(el => el.addEventListener('change', applyFilters));
    document.getElementById('min-diff').addEventListener('input', applyFilters);
    document.querySelectorAll('th.sortable').forEach(th => {{
      th.addEventListener('click', () => {{
        const key = th.dataset.sort;
        if (!key) return;
        if (currentSort.key === key) {{
          currentSort.asc = !currentSort.asc;
        }} else {{
          currentSort.key = key;
          currentSort.asc = (key === 'fixture' || key === 'shot');
        }}
        sortRows();
        applyFilters();
      }});
    }});

    sortRows();
    applyFilters();
  </script>
</body>
</html>
"#,
    total = report.totals.total,
    passed = report.totals.passed,
    failed = report.totals.failed,
    errors = report.totals.errors,
    json_name = html_escape("report.json"),
    rows = rows
  );

  fs::write(&html_path, report_html)
    .map_err(|e| format!("Failed to write {}: {e}", html_path.display()))?;
  Ok(())
}

fn html_escape(input: &str) -> String {
  input
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
    .replace('\'', "&#39;")
}

fn base_url_for(html_path: &Path) -> Result<String, String> {
  let dir = html_path
    .parent()
    .ok_or_else(|| format!("No parent directory for {}", html_path.display()))?;
  Url::from_directory_path(dir)
    .map_err(|_| format!("Failed to build file:// base URL for {}", dir.display()))
    .map(|url| url.to_string())
}

fn render_page(renderer: &mut FastRender, html: &str, shot: &PageShot) -> Result<Vec<u8>, String> {
  let mut options = RenderOptions::new()
    .with_viewport(shot.viewport.0, shot.viewport.1)
    .with_device_pixel_ratio(shot.dpr)
    .with_media_type(shot.media);
  if matches!(shot.media, MediaType::Print) {
    // Print renders are paginated into multiple page fragments; expand the paint canvas so the
    // stacked pages are visible in the regression PNG output.
    options = options.with_fit_canvas_to_content(true);
  }
  if shot.force_parallel_tiling {
    options = options.with_paint_parallelism(PaintParallelism::enabled());
  }

  let pixmap = renderer
    .render_html_with_options(html, options)
    .map_err(|e| format!("Render failed: {:?}", e))?;
  encode_image(&pixmap, OutputFormat::Png).map_err(|e| format!("Encode failed: {:?}", e))
}

fn update_fixture_goldens(fixture: &PageFixture) -> Result<(), String> {
  let html_path = fixtures_dir().join(fixture.html);
  let html = fs::read_to_string(&html_path)
    .map_err(|e| format!("Failed to read {}: {}", html_path.display(), e))?;
  let base_url = base_url_for(&html_path)?;

  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut renderer = FastRender::builder()
    .base_url(base_url)
    .font_sources(FontConfig::bundled_only())
    .resource_policy(policy)
    .build()
    .map_err(|e| format!("Failed to create renderer: {:?}", e))?;

  fs::create_dir_all(golden_dir()).map_err(|e| {
    format!(
      "Failed to create golden dir {}: {}",
      golden_dir().display(),
      e
    )
  })?;

  for shot in fixture.shots {
    let rendered = render_page(&mut renderer, &html, shot)?;
    let golden_name = shot.golden_name(fixture.name);
    let golden_path = golden_path(&golden_name);
    fs::write(&golden_path, &rendered)
      .map_err(|e| format!("Failed to write golden {}: {}", golden_path.display(), e))?;
    eprintln!("Updated golden for {}", golden_name);
  }

  Ok(())
}

fn error_entry(
  fixture: &str,
  shot: &str,
  golden_name: &str,
  compare_config: &PagesCompareConfigSummary,
  message: String,
) -> PagesReportEntry {
  PagesReportEntry {
    fixture: fixture.to_string(),
    shot: shot.to_string(),
    golden_name: golden_name.to_string(),
    status: PagesReportStatus::Error,
    compare_config: compare_config.clone(),
    metrics: None,
    artifacts: None,
    message: Some(message),
  }
}

fn compare_entry(
  fixture: &str,
  shot: &str,
  golden_name: &str,
  compare_config: &CompareConfig,
  compare_summary: &PagesCompareConfigSummary,
  output_dir: &Path,
  rendered_png: &[u8],
  golden_png: &[u8],
) -> PagesReportEntry {
  let actual = match load_png_from_bytes(rendered_png) {
    Ok(actual) => actual,
    Err(e) => {
      return error_entry(
        fixture,
        shot,
        golden_name,
        compare_summary,
        format!("Failed to decode rendered PNG: {e}"),
      )
    }
  };
  let expected = match load_png_from_bytes(golden_png) {
    Ok(expected) => expected,
    Err(e) => {
      return error_entry(
        fixture,
        shot,
        golden_name,
        compare_summary,
        format!("Failed to decode golden PNG: {e}"),
      )
    }
  };

  let diff = compare_images(&actual, &expected, compare_config);
  let metrics = diff.dimensions_match.then(|| PagesCompareMetrics {
    different_pixels: diff.statistics.different_pixels,
    total_pixels: diff.statistics.total_pixels,
    different_percent: diff.statistics.different_percent,
    max_channel_diff: diff.statistics.max_channel_diff(diff.config.compare_alpha),
    perceptual_distance: diff.statistics.perceptual_distance,
  });

  if diff.is_match() {
    return PagesReportEntry {
      fixture: fixture.to_string(),
      shot: shot.to_string(),
      golden_name: golden_name.to_string(),
      status: PagesReportStatus::Pass,
      compare_config: compare_summary.clone(),
      metrics,
      artifacts: None,
      message: None,
    };
  }

  let artifacts_result = save_artifacts(golden_name, rendered_png, golden_png, &diff, output_dir);
  let (artifacts, message) = match artifacts_result {
    Ok(paths) => (
      Some(PagesCompareArtifacts {
        actual: Some(report_rel_path(output_dir, &paths.actual)),
        expected: Some(report_rel_path(output_dir, &paths.expected)),
        diff: paths.diff.map(|p| report_rel_path(output_dir, &p)),
      }),
      Some(diff.summary()),
    ),
    Err(e) => (None, Some(format!("{} (failed to save artifacts: {e})", diff.summary()))),
  };

  PagesReportEntry {
    fixture: fixture.to_string(),
    shot: shot.to_string(),
    golden_name: golden_name.to_string(),
    status: PagesReportStatus::Fail,
    compare_config: compare_summary.clone(),
    metrics,
    artifacts,
    message,
  }
}

fn compare_fixture(
  fixture: &PageFixture,
  base_compare_config: &CompareConfig,
  overrides: &PagesOverrides,
  output_dir: &Path,
) -> Vec<PagesReportEntry> {
  let mut compare_config = base_compare_config.clone();
  overrides.apply_to(fixture.name, &mut compare_config);
  let compare_summary = PagesCompareConfigSummary::from(&compare_config);

  let html_path = fixtures_dir().join(fixture.html);
  let html = match fs::read_to_string(&html_path) {
    Ok(html) => html,
    Err(e) => {
      return fixture
        .shots
        .iter()
        .map(|shot| {
          let golden_name = shot.golden_name(fixture.name);
          error_entry(
            fixture.name,
            shot.label,
            &golden_name,
            &compare_summary,
            format!("Failed to read {}: {}", html_path.display(), e),
          )
        })
        .collect();
    }
  };

  let base_url = match base_url_for(&html_path) {
    Ok(url) => url,
    Err(e) => {
      return fixture
        .shots
        .iter()
        .map(|shot| {
          let golden_name = shot.golden_name(fixture.name);
          error_entry(fixture.name, shot.label, &golden_name, &compare_summary, e.clone())
        })
        .collect();
    }
  };

  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut renderer = match FastRender::builder()
    .base_url(base_url)
    .font_sources(FontConfig::bundled_only())
    .resource_policy(policy)
    .build()
  {
    Ok(renderer) => renderer,
    Err(e) => {
      return fixture
        .shots
        .iter()
        .map(|shot| {
          let golden_name = shot.golden_name(fixture.name);
          error_entry(
            fixture.name,
            shot.label,
            &golden_name,
            &compare_summary,
            format!("Failed to create renderer: {:?}", e),
          )
        })
        .collect();
    }
  };

  let mut entries = Vec::new();
  for shot in fixture.shots {
    let golden_name = shot.golden_name(fixture.name);
    let rendered = match render_page(&mut renderer, &html, shot) {
      Ok(png) => png,
      Err(e) => {
        entries.push(error_entry(
          fixture.name,
          shot.label,
          &golden_name,
          &compare_summary,
          e,
        ));
        continue;
      }
    };

    let golden_path = golden_path(&golden_name);
    let golden = match fs::read(&golden_path) {
      Ok(png) => png,
      Err(e) => {
        entries.push(error_entry(
          fixture.name,
          shot.label,
          &golden_name,
          &compare_summary,
          format!(
            "Missing golden {} ({}). Set UPDATE_PAGES_GOLDEN=1 to regenerate. Error: {}",
            golden_name,
            golden_path.display(),
            e
          ),
        ));
        continue;
      }
    };

    entries.push(compare_entry(
      fixture.name,
      shot.label,
      &golden_name,
      &compare_config,
      &compare_summary,
      output_dir,
      &rendered,
      &golden,
    ));
  }

  entries
}

#[test]
fn pages_overrides_validate_rules() {
  let overrides = load_pages_overrides().expect("failed to load pages overrides");
  let fixture_names: Vec<&str> = PAGE_FIXTURES.iter().map(|f| f.name).collect();

  let mut unknown_exact = Vec::new();
  let mut unmatched_prefix = Vec::new();

  for rule in &overrides.rules {
    match rule.match_kind {
      PagesOverrideMatch::Exact => {
        if !fixture_names.iter().any(|name| *name == rule.fixture) {
          unknown_exact.push(rule.fixture.clone());
        }
      }
      PagesOverrideMatch::Prefix => {
        if !fixture_names.iter().any(|name| name.starts_with(&rule.fixture)) {
          unmatched_prefix.push(rule.fixture.clone());
        }
      }
    }
  }

  unknown_exact.sort();
  unmatched_prefix.sort();

  let mut problems = Vec::new();
  if !unknown_exact.is_empty() {
    problems.push(format!(
      "Exact-match rules reference unknown fixtures:\n{}",
      unknown_exact.join("\n")
    ));
  }
  if !unmatched_prefix.is_empty() {
    problems.push(format!(
      "Prefix-match rules did not match any fixtures:\n{}",
      unmatched_prefix.join("\n")
    ));
  }

  assert!(
    problems.is_empty(),
    "Invalid pages overrides in {}:\n{}",
    pages_overrides_path().display(),
    problems.join("\n\n")
  );
}

#[test]
fn pages_regression_suite() {
  let compare_config =
    compare_config_from_env(CompareEnvVars::pages()).expect("invalid comparison configuration");
  let overrides = load_pages_overrides().unwrap_or_else(|e| panic!("Pages overrides: {e}"));
  let filter = fixture_filter();
  let update_goldens = should_update_goldens();
  let output_dir = pages_output_dir();
  let write_report = pages_report_enabled();
  let max_failures =
    pages_max_failures_from_env().unwrap_or_else(|e| panic!("Pages failure policy: {e}"));

  thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(move || {
      if update_goldens {
        for fixture in PAGE_FIXTURES {
          if let Some(filter) = filter.as_ref() {
            if !filter.iter().any(|name| name == fixture.name) {
              continue;
            }
          }
          update_fixture_goldens(fixture)
            .unwrap_or_else(|e| panic!("Page '{}' failed: {}", fixture.name, e));
        }
        return;
      }

      let mut entries = Vec::new();
      let mut failures = 0usize;
      let mut first_failure = None::<PagesReportEntry>;

      for fixture in PAGE_FIXTURES {
        if let Some(filter) = filter.as_ref() {
          if !filter.iter().any(|name| name == fixture.name) {
            continue;
          }
        }

        let fixture_entries = compare_fixture(fixture, &compare_config, overrides, &output_dir);
        for entry in fixture_entries {
          if entry.status.is_failure() {
            failures += 1;
            if first_failure.is_none() {
              first_failure = Some(entry.clone());
            }
          }
          entries.push(entry);
        }

        if failures >= max_failures {
          break;
        }
      }

      let should_write_report = write_report || failures > 0;
      if should_write_report {
        write_pages_report(&output_dir, entries).unwrap_or_else(|e| {
          panic!(
            "Failed to write pages report under {}: {e}",
            output_dir.display()
          )
        });
      }

      if failures > 0 {
        let (html_path, json_path) = pages_report_paths(&output_dir);
        let mut message = format!(
          "{} pages regression failures. Artifacts under {}. Report: {} (JSON: {}).",
          failures,
          output_dir.display(),
          html_path.display(),
          json_path.display()
        );
        if let Some(first) = first_failure {
          if let Some(detail) = first.message {
            message.push_str(&format!(
              "\nFirst failure: {} ({}) - {}",
              first.fixture, first.shot, detail
            ));
          }
        }
        message.push_str(
          "\nSet PAGES_MAX_FAILURES=<N> or PAGES_FAIL_FAST=0 to keep going and collect more failures.",
        );
        panic!("{}", message);
      }
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn aborting_pages_render_without_panic() {
  const STACK_SIZE: usize = 64 * 1024 * 1024;

  // These fixtures are intentionally large and can OOM in debug builds on resource-limited hosts.
  // Keep the default pages regression loop lightweight; run this smoke test in release or when
  // explicitly opted in for debug.
  if cfg!(debug_assertions) && std::env::var_os("PAGES_SMOKE_IN_DEBUG").is_none() {
    eprintln!("Skipping pages smoke fixtures in debug (set PAGES_SMOKE_IN_DEBUG=1 to run).");
    return;
  }

  let fixtures = ["cnn.com", "figma.com", "ikea.com"];
  thread::Builder::new()
    .stack_size(STACK_SIZE)
    .spawn(move || {
      // Keep this smoke test deterministic and bounded: avoid system font discovery and network
      // fetches so it behaves consistently across developer machines and CI.
      let policy = ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false)
        .allow_file(true)
        .allow_data(true);
      for fixture in fixtures {
        let html_path = fixtures_dir().join(fixture).join("index.html");
        let html = fs::read_to_string(&html_path)
          .unwrap_or_else(|e| panic!("Failed to read {}: {}", html_path.display(), e));
        let base_dir = html_path
          .parent()
          .unwrap_or_else(|| panic!("No parent for {}", html_path.display()));
        let base_url =
          Url::from_directory_path(base_dir).expect("failed to build file:// base url");

        let mut renderer = FastRender::builder()
          .base_url(base_url.to_string())
          .font_sources(FontConfig::bundled_only())
          .resource_policy(policy.clone())
          .build()
          .expect("renderer should build");
        let options = RenderOptions::new().with_viewport(900, 1400);
        renderer
          .render_html_with_options(&html, options)
          .unwrap_or_else(|e| panic!("Fixture '{}' failed to render: {:?}", fixture, e));
      }
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn page_fixtures_present() {
  for fixture in PAGE_FIXTURES {
    let path = fixtures_dir().join(fixture.html);
    assert!(path.exists(), "Fixture HTML missing: {}", path.display());
  }
}

#[test]
fn pageset_failure_fixtures_present() {
  let progress_dir = Path::new("progress/pages");
  let mut missing = Vec::new();

  for entry in fs::read_dir(progress_dir)
    .unwrap_or_else(|e| panic!("Failed to read {}: {}", progress_dir.display(), e))
  {
    let entry = entry.unwrap_or_else(|e| panic!("Failed to read progress entry: {}", e));
    let path = entry.path();

    if path.extension() != Some(std::ffi::OsStr::new("json")) {
      continue;
    }

    let stem = match path.file_stem().and_then(|s| s.to_str()) {
      Some(stem) => stem,
      None => continue,
    };

    let contents = fs::read_to_string(&path)
      .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
    let json: serde_json::Value = serde_json::from_str(&contents)
      .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e));
    let status = json.get("status").and_then(|v| v.as_str()).unwrap_or("ok");
    if status == "ok" {
      continue;
    }

    let fixture_path = fixtures_dir().join(stem).join("index.html");
    if !fixture_path.exists() {
      missing.push(format!("{} ({})", stem, fixture_path.display()));
    }
  }

  assert!(
    missing.is_empty(),
    "progress/pages contains non-ok pages without an offline fixture:\n{}",
    missing.join("\n")
  );
}

#[test]
fn pageset_timeouts_manifest_is_legacy_guardrails_mirror() {
  let guardrails: serde_json::Value =
    serde_json::from_str(include_str!("../pages/pageset_guardrails.json"))
      .expect("failed to parse pageset guardrails manifest");
  let legacy: serde_json::Value =
    serde_json::from_str(include_str!("../pages/pageset_timeouts.json"))
      .expect("failed to parse legacy pageset timeouts manifest");

  assert_eq!(
    guardrails, legacy,
    "pages/pageset_timeouts.json should remain a backwards-compatible mirror of pages/pageset_guardrails.json"
  );
}

mod pageset_guardrails {
  use super::*;

  const MANIFEST_VERSION: u32 = 1;
  const DEFAULT_BUDGET_MS: f64 = 5000.0;
  // Captures the current worst pageset guardrails (timeouts + slow OK hotspots) for offline
  // perf/regression tracking.
  const GUARDRAILS_MANIFEST: &str = include_str!("../pages/pageset_guardrails.json");

  #[derive(Deserialize)]
  struct GuardrailsManifest {
    schema_version: u32,
    #[serde(default)]
    default_budget_ms: Option<f64>,
    fixtures: Vec<GuardrailsFixture>,
  }

  #[derive(Deserialize)]
  struct GuardrailsFixture {
    name: String,
    viewport: [u32; 2],
    dpr: f32,
    media: String,
    #[serde(default)]
    budget_ms: Option<f64>,
  }

  struct GuardrailsRun {
    name: String,
    elapsed_ms: f64,
    budget_ms: f64,
    timings: RenderStageTimings,
  }

  #[test]
  fn pageset_guardrails_fixtures_present() {
    let manifest = load_manifest().expect("failed to parse pageset guardrails manifest");
    let mut missing = Vec::new();

    for fixture in &manifest.fixtures {
      let html_path = fixtures_dir().join(&fixture.name).join("index.html");
      if !html_path.exists() {
        missing.push(format!("{} ({})", fixture.name, html_path.display()));
      }
    }

    assert!(
      missing.is_empty(),
      "pages/pageset_guardrails.json references fixtures missing from the repository:\n{}",
      missing.join("\n")
    );
  }

  #[test]
  fn pageset_guardrails_render_under_budget() {
    if cfg!(debug_assertions)
      && std::env::var_os("PAGESET_GUARDRAILS_IN_DEBUG").is_none()
      && std::env::var_os("PAGESET_TIMEOUTS_IN_DEBUG").is_none()
    {
      eprintln!(
        "Skipping pageset guardrails fixtures in debug (set PAGESET_GUARDRAILS_IN_DEBUG=1 to run)."
      );
      return;
    }

    let manifest = load_manifest().expect("failed to parse pageset guardrails manifest");
    let default_budget_ms = manifest.default_budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
    let global_budget = budget_override_from_env();

    thread::Builder::new()
      .stack_size(64 * 1024 * 1024)
      .spawn(move || {
        for fixture in &manifest.fixtures {
          let html_path = fixtures_dir().join(&fixture.name).join("index.html");
          assert!(
            html_path.exists(),
            "Fixture HTML missing: {} ({})",
            fixture.name,
            html_path.display()
          );

          let run = render_guardrails_fixture(fixture, default_budget_ms, global_budget)
            .unwrap_or_else(|e| panic!("Failed to render {}: {}", fixture.name, e));
          assert!(
            run.elapsed_ms <= run.budget_ms,
            "Fixture {} exceeded budget ({:.1}ms > {:.1}ms). Timings: {}",
            run.name,
            run.elapsed_ms,
            run.budget_ms,
            summarize_timings(&run.timings),
          );
        }
      })
      .unwrap()
      .join()
      .unwrap();
  }

  fn load_manifest() -> Result<GuardrailsManifest, String> {
    let manifest: GuardrailsManifest =
      serde_json::from_str(GUARDRAILS_MANIFEST).map_err(|e| format!("invalid manifest: {e}"))?;
    if manifest.schema_version != MANIFEST_VERSION {
      return Err(format!(
        "unexpected manifest schema_version {}, expected {}",
        manifest.schema_version, MANIFEST_VERSION
      ));
    }
    Ok(manifest)
  }

  fn render_guardrails_fixture(
    fixture: &GuardrailsFixture,
    default_budget_ms: f64,
    global_budget_ms: Option<f64>,
  ) -> Result<GuardrailsRun, String> {
    if fixture.viewport.len() != 2 {
      return Err(format!(
        "fixture {} viewport must have exactly two entries",
        fixture.name
      ));
    }

    let html_path = fixtures_dir().join(&fixture.name).join("index.html");
    let html = fs::read_to_string(&html_path)
      .map_err(|e| format!("failed to read {}: {}", html_path.display(), e))?;
    let base_url = base_url_for(&html_path)?;
    let media = media_from_label(&fixture.media)?;

    let policy = ResourcePolicy::default()
      .allow_http(false)
      .allow_https(false)
      .allow_file(true)
      .allow_data(true);

    let mut renderer = FastRender::builder()
      .viewport_size(fixture.viewport[0], fixture.viewport[1])
      .device_pixel_ratio(fixture.dpr)
      .base_url(base_url.clone())
      .font_sources(FontConfig::bundled_only())
      .resource_policy(policy)
      .build()
      .map_err(|e| format!("failed to build renderer for {}: {:?}", fixture.name, e))?;

    let options = RenderOptions::new()
      .with_viewport(fixture.viewport[0], fixture.viewport[1])
      .with_device_pixel_ratio(fixture.dpr)
      .with_media_type(media)
      .with_diagnostics_level(DiagnosticsLevel::Basic);

    let budget_ms = global_budget_ms
      .or(fixture.budget_ms)
      .unwrap_or(default_budget_ms);

    let start = Instant::now();
    let rendered = renderer
      .render_html_with_diagnostics(&html, options)
      .map_err(|e| format!("render failed for {}: {:?}", fixture.name, e))?;
    let (_, diagnostics) = rendered
      .encode(OutputFormat::Png)
      .map_err(|e| format!("encode failed for {}: {:?}", fixture.name, e))?;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    let stats = diagnostics.stats.ok_or_else(|| {
      format!(
        "diagnostics missing stats for {}; expected DiagnosticsLevel::Basic",
        fixture.name
      )
    })?;

    Ok(GuardrailsRun {
      name: fixture.name.clone(),
      elapsed_ms,
      budget_ms,
      timings: stats.timings,
    })
  }

  fn summarize_timings(timings: &RenderStageTimings) -> String {
    let mut parts = Vec::new();
    for (label, value) in stage_entries(timings) {
      if value > 0.0 {
        parts.push(format!("{label}={value:.1}ms"));
      }
    }
    parts.join(", ")
  }

  fn stage_entries(timings: &RenderStageTimings) -> [(&'static str, f64); 11] {
    [
      ("html_decode", timings.html_decode_ms.unwrap_or(0.0)),
      ("dom_parse", timings.dom_parse_ms.unwrap_or(0.0)),
      ("css_inlining", timings.css_inlining_ms.unwrap_or(0.0)),
      ("css_parse", timings.css_parse_ms.unwrap_or(0.0)),
      ("cascade", timings.cascade_ms.unwrap_or(0.0)),
      ("box_tree", timings.box_tree_ms.unwrap_or(0.0)),
      ("layout", timings.layout_ms.unwrap_or(0.0)),
      ("paint_build", timings.paint_build_ms.unwrap_or(0.0)),
      ("paint_optimize", timings.paint_optimize_ms.unwrap_or(0.0)),
      ("paint_rasterize", timings.paint_rasterize_ms.unwrap_or(0.0)),
      ("encode", timings.encode_ms.unwrap_or(0.0)),
    ]
  }

  fn media_from_label(label: &str) -> Result<MediaType, String> {
    match label.to_ascii_lowercase().as_str() {
      "all" => Ok(MediaType::All),
      "screen" => Ok(MediaType::Screen),
      "print" => Ok(MediaType::Print),
      "speech" => Ok(MediaType::Speech),
      other => Err(format!("unsupported media type \"{other}\"")),
    }
  }

  fn budget_override_from_env() -> Option<f64> {
    std::env::var("PAGESET_GUARDRAILS_BUDGET_MS")
      .or_else(|_| std::env::var("PAGESET_TIMEOUT_BUDGET_MS"))
      .ok()
      .and_then(|value| value.parse::<f64>().ok())
  }
}
