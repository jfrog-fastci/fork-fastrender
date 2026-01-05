mod common;

use clap::Parser;
use common::report::{
  display_path, ensure_parent_dir, entry_anchor_id, escape_html, format_linked_image, path_for_report,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 2;
const METRIC_EPS: f64 = 1e-9;
const TOP_N: usize = 20;

#[derive(Parser, Debug)]
#[command(
  name = "compare_diff_reports",
  about = "Compare two diff_renders JSON reports and summarize accuracy deltas"
)]
struct Args {
  /// Baseline diff_report.json
  #[arg(long, value_name = "PATH")]
  baseline: PathBuf,

  /// New diff_report.json
  #[arg(long = "new", value_name = "PATH")]
  new_report: PathBuf,

  /// Optional baseline diff_report.html (used for report links and resolving image paths).
  #[arg(long, value_name = "PATH")]
  baseline_html: Option<PathBuf>,

  /// Optional new diff_report.html (used for report links and resolving image paths).
  #[arg(long, value_name = "PATH")]
  new_html: Option<PathBuf>,

  /// Path to write diff_report_delta.json
  #[arg(long, default_value = "diff_report_delta.json")]
  json: PathBuf,

  /// Path to write diff_report_delta.html
  #[arg(long, default_value = "diff_report_delta.html")]
  html: PathBuf,

  /// Proceed even when the diff report comparison config differs.
  #[arg(long)]
  allow_config_mismatch: bool,

  /// Exit non-zero when any entry regresses (respecting `--regression-threshold-percent`).
  #[arg(long)]
  fail_on_regression: bool,

  /// Only treat an entry as a failing regression when diff_percentage increases by more than this amount.
  #[arg(long, default_value_t = 0.0, value_name = "PERCENT")]
  regression_threshold_percent: f64,

  /// Only compare entries whose names match this regex (can be repeated).
  #[arg(long, value_name = "REGEX")]
  include: Vec<String>,

  /// Exclude entries whose names match this regex (can be repeated).
  #[arg(long, value_name = "REGEX")]
  exclude: Vec<String>,
}

struct NameFilters {
  include: Vec<Regex>,
  exclude: Vec<Regex>,
}

impl NameFilters {
  fn matches(&self, name: &str) -> bool {
    if !self.include.is_empty() && !self.include.iter().any(|re| re.is_match(name)) {
      return false;
    }
    if self.exclude.iter().any(|re| re.is_match(name)) {
      return false;
    }
    true
  }
}

#[derive(Deserialize, Clone)]
struct DiffReport {
  before_dir: String,
  after_dir: String,
  tolerance: u8,
  max_diff_percent: f64,
  max_perceptual_distance: Option<f64>,
  #[serde(default)]
  ignore_alpha: bool,
  shard: Option<DiffReportShard>,
  results: Vec<DiffReportEntry>,
}

#[derive(Deserialize, Clone)]
struct DiffReportShard {
  index: usize,
  total: usize,
  #[allow(dead_code)]
  discovered: usize,
}

#[derive(Deserialize, Clone)]
struct DiffReportEntry {
  name: String,
  status: EntryStatus,
  before: Option<String>,
  after: Option<String>,
  diff: Option<String>,
  metrics: Option<MetricsSummary>,
  error: Option<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryStatus {
  Match,
  WithinThreshold,
  Diff,
  MissingBefore,
  MissingAfter,
  Error,
}

impl EntryStatus {
  fn kind_weight(&self) -> u8 {
    match self {
      EntryStatus::Match | EntryStatus::WithinThreshold | EntryStatus::Diff => 0,
      EntryStatus::MissingBefore | EntryStatus::MissingAfter => 1,
      EntryStatus::Error => 2,
    }
  }

  fn label(&self) -> &'static str {
    match self {
      EntryStatus::Match => "match",
      EntryStatus::WithinThreshold => "within-threshold",
      EntryStatus::Diff => "diff",
      EntryStatus::MissingBefore => "missing-before",
      EntryStatus::MissingAfter => "missing-after",
      EntryStatus::Error => "error",
    }
  }
}

#[derive(Deserialize, Clone, Copy, Serialize)]
struct MetricsSummary {
  diff_percentage: f64,
  perceptual_distance: f64,
  #[serde(default)]
  pixel_diff: u64,
  #[serde(default)]
  total_pixels: u64,
}

#[derive(Serialize)]
struct DeltaReport {
  schema_version: u32,
  baseline: ReportMeta,
  new: ReportMeta,
  #[serde(skip_serializing_if = "Option::is_none")]
  filters: Option<ReportFilters>,
  #[serde(skip_serializing_if = "Option::is_none")]
  gating: Option<ReportGating>,
  config_mismatches: Vec<ConfigMismatch>,
  totals: DeltaTotals,
  aggregate: AggregateMetrics,
  top_improvements: Vec<DeltaRankedEntry>,
  top_regressions: Vec<DeltaRankedEntry>,
  results: Vec<DeltaEntry>,
}

#[derive(Serialize)]
struct ReportFilters {
  #[serde(skip_serializing_if = "Vec::is_empty")]
  include: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  exclude: Vec<String>,
  matched_entries: usize,
  total_entries: usize,
}

#[derive(Serialize, Clone)]
struct ReportGating {
  fail_on_regression: bool,
  regression_threshold_percent: f64,
}

#[derive(Serialize)]
struct ReportMeta {
  report_json: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  report_html: Option<String>,
  before_dir: String,
  after_dir: String,
  tolerance: u8,
  max_diff_percent: f64,
  max_perceptual_distance: Option<f64>,
  ignore_alpha: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  shard: Option<ReportShard>,
}

#[derive(Serialize)]
struct ReportShard {
  index: usize,
  total: usize,
}

#[derive(Serialize)]
struct ConfigMismatch {
  field: &'static str,
  baseline: String,
  new: String,
}

#[derive(Serialize, Default)]
struct DeltaTotals {
  entries: usize,
  paired: usize,
  improved: usize,
  regressed: usize,
  unchanged: usize,
  missing_in_baseline: usize,
  missing_in_new: usize,
  baseline_errors: usize,
  new_errors: usize,
  baseline_missing: usize,
  new_missing: usize,
}

#[derive(Serialize)]
struct DeltaEntry {
  name: String,
  baseline: Option<EntrySummary>,
  new: Option<EntrySummary>,
  #[serde(skip_serializing_if = "Option::is_none")]
  diff_percentage_delta: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  perceptual_distance_delta: Option<f64>,
  classification: DeltaClassification,
  #[serde(skip_serializing_if = "std::ops::Not::not")]
  failing_regression: bool,
}

#[derive(Serialize, Clone)]
struct EntrySummary {
  status: EntryStatus,
  #[serde(skip_serializing_if = "Option::is_none")]
  before: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  after: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  diff: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  metrics: Option<MetricsSummary>,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DeltaClassification {
  Improved,
  Regressed,
  Unchanged,
  MissingInBaseline,
  MissingInNew,
}

impl DeltaClassification {
  fn label(&self) -> &'static str {
    match self {
      DeltaClassification::Improved => "improved",
      DeltaClassification::Regressed => "regressed",
      DeltaClassification::Unchanged => "unchanged",
      DeltaClassification::MissingInBaseline => "missing-in-baseline",
      DeltaClassification::MissingInNew => "missing-in-new",
    }
  }

  fn row_class(&self) -> &'static str {
    match self {
      DeltaClassification::Improved => "improved",
      DeltaClassification::Regressed => "regressed",
      DeltaClassification::Unchanged => "unchanged",
      DeltaClassification::MissingInBaseline | DeltaClassification::MissingInNew => "missing",
    }
  }

  fn sort_weight(&self) -> u8 {
    match self {
      DeltaClassification::MissingInNew => 0,
      DeltaClassification::Regressed => 1,
      DeltaClassification::MissingInBaseline => 2,
      DeltaClassification::Improved => 3,
      DeltaClassification::Unchanged => 4,
    }
  }
}

#[derive(Serialize)]
struct DeltaRankedEntry {
  name: String,
  diff_percentage_delta: f64,
  perceptual_distance_delta: f64,
}

#[derive(Serialize, Default)]
struct AggregateMetrics {
  paired_with_metrics: usize,
  baseline: AggregateSideMetrics,
  new: AggregateSideMetrics,
  delta: AggregateDeltaMetrics,
}

#[derive(Serialize, Default)]
struct AggregateSideMetrics {
  total_pixels: u64,
  pixel_diff: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  weighted_diff_percentage: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  mean_diff_percentage: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  mean_perceptual_distance: Option<f64>,
}

#[derive(Serialize, Default)]
struct AggregateDeltaMetrics {
  #[serde(skip_serializing_if = "Option::is_none")]
  weighted_diff_percentage: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  mean_diff_percentage: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  mean_perceptual_distance: Option<f64>,
}

fn main() {
  match run() {
    Ok(exit_code) => std::process::exit(exit_code),
    Err(err) => {
      eprintln!("error: {err}");
      std::process::exit(1);
    }
  }
}

fn run() -> Result<i32, String> {
  let args = Args::parse();
  let name_filters = compile_name_filters(&args)?;
  let filter_patterns = if args.include.is_empty() && args.exclude.is_empty() {
    None
  } else {
    Some((args.include.clone(), args.exclude.clone()))
  };
  let gating_meta =
    if args.fail_on_regression || args.regression_threshold_percent.abs() > METRIC_EPS {
      Some(ReportGating {
        fail_on_regression: args.fail_on_regression,
        regression_threshold_percent: args.regression_threshold_percent,
      })
    } else {
      None
    };

  let cwd =
    std::env::current_dir().map_err(|e| format!("failed to read current directory: {e}"))?;
  let baseline_path =
    fs::canonicalize(&args.baseline).map_err(|e| format!("{}: {e}", args.baseline.display()))?;
  let new_path = fs::canonicalize(&args.new_report)
    .map_err(|e| format!("{}: {e}", args.new_report.display()))?;

  let baseline_html_override = args
    .baseline_html
    .as_ref()
    .map(|path| absolutize_path(&cwd, path));
  let new_html_override = args
    .new_html
    .as_ref()
    .map(|path| absolutize_path(&cwd, path));
  let baseline_report_html = baseline_html_override
    .clone()
    .or_else(|| guess_report_html_path(&baseline_path));
  let new_report_html = new_html_override
    .clone()
    .or_else(|| guess_report_html_path(&new_path));

  let baseline_report = read_report(&baseline_path)?;
  let new_report = read_report(&new_path)?;

  let baseline_meta = ReportMeta::from_report(
    &baseline_report,
    &baseline_path,
    baseline_report_html.as_deref(),
  );
  let new_meta = ReportMeta::from_report(&new_report, &new_path, new_report_html.as_deref());

  let config_mismatches = diff_config(&baseline_report, &new_report);
  if !config_mismatches.is_empty() {
    if args.allow_config_mismatch {
      eprintln!("warning: diff report config mismatch (--allow-config-mismatch set; continuing):");
    } else {
      eprintln!("warning: diff report config mismatch (pass --allow-config-mismatch to proceed):");
    }
    for mismatch in &config_mismatches {
      eprintln!(
        "  - {}: baseline={} new={}",
        mismatch.field, mismatch.baseline, mismatch.new
      );
    }
    if !args.allow_config_mismatch {
      let totals = compute_totals_without_deltas(&baseline_report, &new_report, &name_filters);
      let mut total_names: BTreeSet<&str> = BTreeSet::new();
      total_names.extend(baseline_report.results.iter().map(|e| e.name.as_str()));
      total_names.extend(new_report.results.iter().map(|e| e.name.as_str()));
      let total_entries = total_names.len();
      let filters_meta = filter_patterns
        .as_ref()
        .map(|(include, exclude)| ReportFilters {
          include: include.clone(),
          exclude: exclude.clone(),
          matched_entries: totals.entries,
          total_entries,
        });
      let report = DeltaReport {
        schema_version: SCHEMA_VERSION,
        baseline: baseline_meta,
        new: new_meta,
        filters: filters_meta,
        gating: gating_meta.clone(),
        config_mismatches,
        totals,
        aggregate: AggregateMetrics::default(),
        top_improvements: Vec::new(),
        top_regressions: Vec::new(),
        results: Vec::new(),
      };

      write_json_report(&report, &args.json)?;
      write_html_report(
        &report,
        &baseline_path,
        &new_path,
        &args.html,
        baseline_html_override.as_deref(),
        new_html_override.as_deref(),
      )?;
      println!(
        "Refusing to compare reports with mismatched diff config; wrote delta report to {} and {}.",
        args.json.display(),
        args.html.display()
      );
      return Ok(1);
    }
  }

  let baseline_by_name = index_entries(baseline_report.results);
  let new_by_name = index_entries(new_report.results);

  let mut names = BTreeSet::new();
  names.extend(baseline_by_name.keys().cloned());
  names.extend(new_by_name.keys().cloned());
  let total_entries = names.len();
  let names: Vec<String> = names
    .into_iter()
    .filter(|name| name_filters.matches(name))
    .collect();

  let mut totals = DeltaTotals::default();
  totals.entries = names.len();

  let mut results = Vec::with_capacity(names.len());

  for name in names {
    let baseline_entry = baseline_by_name.get(&name);
    let new_entry = new_by_name.get(&name);

    let baseline_summary = baseline_entry.map(to_entry_summary);
    let new_summary = new_entry.map(to_entry_summary);

    let (diff_percentage_delta, perceptual_distance_delta, classification) =
      classify_delta(baseline_entry, new_entry);

    if baseline_entry.is_some() && new_entry.is_some() {
      totals.paired += 1;
    } else if baseline_entry.is_none() {
      totals.missing_in_baseline += 1;
    } else {
      totals.missing_in_new += 1;
    }

    if let Some(entry) = baseline_entry {
      match entry.status {
        EntryStatus::Error => totals.baseline_errors += 1,
        EntryStatus::MissingBefore | EntryStatus::MissingAfter => totals.baseline_missing += 1,
        _ => {}
      }
    }
    if let Some(entry) = new_entry {
      match entry.status {
        EntryStatus::Error => totals.new_errors += 1,
        EntryStatus::MissingBefore | EntryStatus::MissingAfter => totals.new_missing += 1,
        _ => {}
      }
    }

    match classification {
      DeltaClassification::Improved => totals.improved += 1,
      DeltaClassification::Regressed => totals.regressed += 1,
      DeltaClassification::Unchanged => totals.unchanged += 1,
      DeltaClassification::MissingInBaseline | DeltaClassification::MissingInNew => {}
    }

    let mut entry = DeltaEntry {
      name,
      baseline: baseline_summary,
      new: new_summary,
      diff_percentage_delta,
      perceptual_distance_delta,
      classification,
      failing_regression: false,
    };
    if args.fail_on_regression {
      entry.failing_regression = is_failing_regression(&entry, args.regression_threshold_percent);
    }
    results.push(entry);
  }

  sort_results(&mut results);

  let mut top_improvements = collect_top_metric_deltas(&results, true);
  let mut top_regressions = collect_top_metric_deltas(&results, false);
  top_improvements.truncate(TOP_N);
  top_regressions.truncate(TOP_N);

  let aggregate = compute_aggregate_metrics(&results);
  let filters_meta = filter_patterns
    .as_ref()
    .map(|(include, exclude)| ReportFilters {
      include: include.clone(),
      exclude: exclude.clone(),
      matched_entries: totals.entries,
      total_entries,
    });
  let report = DeltaReport {
    schema_version: SCHEMA_VERSION,
    baseline: baseline_meta,
    new: new_meta,
    filters: filters_meta,
    gating: gating_meta,
    config_mismatches,
    totals,
    aggregate,
    top_improvements,
    top_regressions,
    results,
  };

  write_json_report(&report, &args.json)?;
  write_html_report(
    &report,
    &baseline_path,
    &new_path,
    &args.html,
    baseline_html_override.as_deref(),
    new_html_override.as_deref(),
  )?;
  print_summary(&report, &args);

  if args.fail_on_regression {
    let threshold = args.regression_threshold_percent;
    let failing: Vec<&DeltaEntry> = report
      .results
      .iter()
      .filter(|entry| entry.failing_regression)
      .collect();
    if !failing.is_empty() {
      eprintln!(
        "{} failing regressions (diff threshold {:.4}%)",
        failing.len(),
        threshold
      );
      print_failing_regressions(&failing, threshold);
      return Ok(1);
    }
  }

  Ok(0)
}

fn sort_results(entries: &mut [DeltaEntry]) {
  use std::cmp::Ordering;

  entries.sort_by(|a, b| {
    let a_weight = a.classification.sort_weight();
    let b_weight = b.classification.sort_weight();
    if a_weight != b_weight {
      return a_weight.cmp(&b_weight);
    }

    match a.classification {
      DeltaClassification::Regressed => compare_regressed(a, b),
      DeltaClassification::Improved => compare_improved(a, b),
      _ => a.name.cmp(&b.name),
    }
  });

  fn compare_regressed(a: &DeltaEntry, b: &DeltaEntry) -> Ordering {
    use std::cmp::Ordering;

    let a_missing = a.diff_percentage_delta.is_none();
    let b_missing = b.diff_percentage_delta.is_none();
    if a_missing != b_missing {
      return a_missing.cmp(&b_missing).reverse();
    }

    let diff_order = match (a.diff_percentage_delta, b.diff_percentage_delta) {
      (Some(a), Some(b)) => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
      _ => Ordering::Equal,
    };
    if diff_order != Ordering::Equal {
      return diff_order;
    }

    let perceptual_order = match (a.perceptual_distance_delta, b.perceptual_distance_delta) {
      (Some(a), Some(b)) => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
      _ => Ordering::Equal,
    };
    if perceptual_order != Ordering::Equal {
      return perceptual_order;
    }

    a.name.cmp(&b.name)
  }

  fn compare_improved(a: &DeltaEntry, b: &DeltaEntry) -> Ordering {
    use std::cmp::Ordering;

    let a_missing = a.diff_percentage_delta.is_none();
    let b_missing = b.diff_percentage_delta.is_none();
    if a_missing != b_missing {
      return a_missing.cmp(&b_missing);
    }

    let diff_order = match (a.diff_percentage_delta, b.diff_percentage_delta) {
      (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
      _ => Ordering::Equal,
    };
    if diff_order != Ordering::Equal {
      return diff_order;
    }

    let perceptual_order = match (a.perceptual_distance_delta, b.perceptual_distance_delta) {
      (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
      _ => Ordering::Equal,
    };
    if perceptual_order != Ordering::Equal {
      return perceptual_order;
    }

    a.name.cmp(&b.name)
  }
}

fn compile_name_filters(args: &Args) -> Result<NameFilters, String> {
  if !args.regression_threshold_percent.is_finite() || args.regression_threshold_percent < 0.0 {
    return Err("--regression-threshold-percent must be a finite, non-negative number".to_string());
  }

  let include = args
    .include
    .iter()
    .map(|pattern| {
      Regex::new(pattern).map_err(|e| format!("invalid --include regex {pattern:?}: {e}"))
    })
    .collect::<Result<Vec<_>, _>>()?;
  let exclude = args
    .exclude
    .iter()
    .map(|pattern| {
      Regex::new(pattern).map_err(|e| format!("invalid --exclude regex {pattern:?}: {e}"))
    })
    .collect::<Result<Vec<_>, _>>()?;

  Ok(NameFilters { include, exclude })
}

fn print_summary(report: &DeltaReport, args: &Args) {
  println!(
    "Report delta: paired={} improved={} regressed={} unchanged={} missing_in_baseline={} missing_in_new={} baseline_errors={} new_errors={} baseline_missing={} new_missing={}",
    report.totals.paired,
    report.totals.improved,
    report.totals.regressed,
    report.totals.unchanged,
    report.totals.missing_in_baseline,
    report.totals.missing_in_new,
    report.totals.baseline_errors,
    report.totals.new_errors,
    report.totals.baseline_missing,
    report.totals.new_missing,
  );

  if let Some(gating) = &report.gating {
    println!(
      "Gating: fail_on_regression={} regression_threshold_percent={:.4}%",
      gating.fail_on_regression, gating.regression_threshold_percent
    );
    if gating.fail_on_regression {
      let failing = report
        .results
        .iter()
        .filter(|entry| entry.failing_regression)
        .count();
      println!("Failing regressions: {failing}");
    }
  }

  if let Some(filters) = &report.filters {
    if !filters.include.is_empty() || !filters.exclude.is_empty() {
      let include = if filters.include.is_empty() {
        "-".to_string()
      } else {
        filters.include.join(", ")
      };
      let exclude = if filters.exclude.is_empty() {
        "-".to_string()
      } else {
        filters.exclude.join(", ")
      };
      println!(
        "Filters: include=[{}] exclude=[{}] matched={}/{}",
        include, exclude, filters.matched_entries, filters.total_entries
      );
    }
  }

  if report.baseline.shard.is_some() || report.new.shard.is_some() {
    println!(
      "Shard (baseline/new): {}/{}",
      shard_label(&report.baseline.shard),
      shard_label(&report.new.shard),
    );
  }

  if report.aggregate.paired_with_metrics > 0 {
    let weighted = report
      .aggregate
      .delta
      .weighted_diff_percentage
      .map(|v| format!("{:+.4}%", v))
      .unwrap_or_else(|| "-".to_string());
    let mean = report
      .aggregate
      .delta
      .mean_diff_percentage
      .map(|v| format!("{:+.4}%", v))
      .unwrap_or_else(|| "-".to_string());
    let perceptual = report
      .aggregate
      .delta
      .mean_perceptual_distance
      .map(|v| format!("{:+.4}", v))
      .unwrap_or_else(|| "-".to_string());

    println!(
      "Aggregate delta ({} paired with metrics): weighted_diff={} mean_diff={} mean_perceptual={}",
      report.aggregate.paired_with_metrics, weighted, mean, perceptual
    );
  }

  println!(
    "Wrote delta reports: json={} html={}",
    args.json.display(),
    args.html.display()
  );
}

fn shard_label(shard: &Option<ReportShard>) -> String {
  shard
    .as_ref()
    .map(|s| format!("{}/{}", s.index, s.total))
    .unwrap_or_else(|| "-".to_string())
}

fn absolutize_path(cwd: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    cwd.join(path)
  }
}

fn print_failing_regressions(failing: &[&DeltaEntry], threshold: f64) {
  const LIMIT: usize = 20;

  let mut missing_in_new = Vec::new();
  let mut missing_metrics = Vec::new();
  let mut metric_regressions = Vec::new();

  for entry in failing {
    match entry.classification {
      DeltaClassification::MissingInNew => missing_in_new.push(entry),
      DeltaClassification::Regressed => {
        if entry.diff_percentage_delta.is_none() {
          missing_metrics.push(entry);
        } else {
          metric_regressions.push(entry);
        }
      }
      _ => {}
    }
  }

  if !missing_in_new.is_empty() {
    eprintln!("Missing in new report:");
    for entry in missing_in_new.iter().take(LIMIT) {
      eprintln!("  - {}", entry.name);
    }
    if missing_in_new.len() > LIMIT {
      eprintln!("  ... ({} more)", missing_in_new.len() - LIMIT);
    }
  }

  if !missing_metrics.is_empty() {
    eprintln!("Regressed without comparable metrics:");
    for entry in missing_metrics.iter().take(LIMIT) {
      let baseline = entry
        .baseline
        .as_ref()
        .map(|s| s.status.label())
        .unwrap_or("-");
      let new_status = entry.new.as_ref().map(|s| s.status.label()).unwrap_or("-");
      eprintln!(
        "  - {} (baseline={}, new={})",
        entry.name, baseline, new_status
      );
    }
    if missing_metrics.len() > LIMIT {
      eprintln!("  ... ({} more)", missing_metrics.len() - LIMIT);
    }
  }

  if !metric_regressions.is_empty() {
    metric_regressions.sort_by(|a, b| {
      let a_delta = a.diff_percentage_delta.unwrap_or(0.0);
      let b_delta = b.diff_percentage_delta.unwrap_or(0.0);
      b_delta
        .partial_cmp(&a_delta)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.name.cmp(&b.name))
    });

    eprintln!(
      "Failing regressions with metrics (diff threshold {:.4}%):",
      threshold
    );
    for entry in metric_regressions.iter().take(LIMIT) {
      let diff = entry
        .diff_percentage_delta
        .map(|d| format!("{:+.4}%", d))
        .unwrap_or_else(|| "-".to_string());
      let perceptual = entry
        .perceptual_distance_delta
        .map(|d| format!("{:+.4}", d))
        .unwrap_or_else(|| "-".to_string());
      eprintln!(
        "  - {}: Δdiff={} Δperceptual={}",
        entry.name, diff, perceptual
      );
    }
    if metric_regressions.len() > LIMIT {
      eprintln!("  ... ({} more)", metric_regressions.len() - LIMIT);
    }
  }
}

fn compute_aggregate_metrics(entries: &[DeltaEntry]) -> AggregateMetrics {
  let mut paired = 0usize;

  let mut baseline_total_pixels = 0u64;
  let mut baseline_pixel_diff = 0u64;
  let mut baseline_diff_sum = 0.0;
  let mut baseline_perceptual_sum = 0.0;

  let mut new_total_pixels = 0u64;
  let mut new_pixel_diff = 0u64;
  let mut new_diff_sum = 0.0;
  let mut new_perceptual_sum = 0.0;

  for entry in entries {
    let Some(bm) = entry.baseline.as_ref().and_then(|s| s.metrics) else {
      continue;
    };
    let Some(nm) = entry.new.as_ref().and_then(|s| s.metrics) else {
      continue;
    };

    paired += 1;

    baseline_total_pixels = baseline_total_pixels.saturating_add(bm.total_pixels);
    baseline_pixel_diff = baseline_pixel_diff.saturating_add(bm.pixel_diff);
    baseline_diff_sum += bm.diff_percentage;
    baseline_perceptual_sum += bm.perceptual_distance;

    new_total_pixels = new_total_pixels.saturating_add(nm.total_pixels);
    new_pixel_diff = new_pixel_diff.saturating_add(nm.pixel_diff);
    new_diff_sum += nm.diff_percentage;
    new_perceptual_sum += nm.perceptual_distance;
  }

  let paired_f64 = paired as f64;

  let baseline_weighted_diff = if baseline_total_pixels > 0 {
    Some(baseline_pixel_diff as f64 / baseline_total_pixels as f64 * 100.0)
  } else {
    None
  };
  let new_weighted_diff = if new_total_pixels > 0 {
    Some(new_pixel_diff as f64 / new_total_pixels as f64 * 100.0)
  } else {
    None
  };

  let baseline_mean_diff = if paired > 0 {
    Some(baseline_diff_sum / paired_f64)
  } else {
    None
  };
  let new_mean_diff = if paired > 0 {
    Some(new_diff_sum / paired_f64)
  } else {
    None
  };

  let baseline_mean_perceptual = if paired > 0 {
    Some(baseline_perceptual_sum / paired_f64)
  } else {
    None
  };
  let new_mean_perceptual = if paired > 0 {
    Some(new_perceptual_sum / paired_f64)
  } else {
    None
  };

  AggregateMetrics {
    paired_with_metrics: paired,
    baseline: AggregateSideMetrics {
      total_pixels: baseline_total_pixels,
      pixel_diff: baseline_pixel_diff,
      weighted_diff_percentage: baseline_weighted_diff,
      mean_diff_percentage: baseline_mean_diff,
      mean_perceptual_distance: baseline_mean_perceptual,
    },
    new: AggregateSideMetrics {
      total_pixels: new_total_pixels,
      pixel_diff: new_pixel_diff,
      weighted_diff_percentage: new_weighted_diff,
      mean_diff_percentage: new_mean_diff,
      mean_perceptual_distance: new_mean_perceptual,
    },
    delta: AggregateDeltaMetrics {
      weighted_diff_percentage: match (baseline_weighted_diff, new_weighted_diff) {
        (Some(baseline), Some(new)) => Some(new - baseline),
        _ => None,
      },
      mean_diff_percentage: match (baseline_mean_diff, new_mean_diff) {
        (Some(baseline), Some(new)) => Some(new - baseline),
        _ => None,
      },
      mean_perceptual_distance: match (baseline_mean_perceptual, new_mean_perceptual) {
        (Some(baseline), Some(new)) => Some(new - baseline),
        _ => None,
      },
    },
  }
}

fn compute_totals_without_deltas(
  baseline: &DiffReport,
  new_report: &DiffReport,
  filters: &NameFilters,
) -> DeltaTotals {
  let baseline_names: BTreeSet<String> = baseline
    .results
    .iter()
    .map(|e| e.name.clone())
    .filter(|name| filters.matches(name))
    .collect();
  let new_names: BTreeSet<String> = new_report
    .results
    .iter()
    .map(|e| e.name.clone())
    .filter(|name| filters.matches(name))
    .collect();

  let entries = baseline_names.union(&new_names).count();
  let paired = baseline_names.intersection(&new_names).count();
  let missing_in_baseline = new_names.difference(&baseline_names).count();
  let missing_in_new = baseline_names.difference(&new_names).count();

  let baseline_errors = baseline
    .results
    .iter()
    .filter(|entry| filters.matches(&entry.name))
    .filter(|entry| matches!(entry.status, EntryStatus::Error))
    .count();
  let new_errors = new_report
    .results
    .iter()
    .filter(|entry| filters.matches(&entry.name))
    .filter(|entry| matches!(entry.status, EntryStatus::Error))
    .count();

  let baseline_missing = baseline
    .results
    .iter()
    .filter(|entry| filters.matches(&entry.name))
    .filter(|entry| {
      matches!(
        entry.status,
        EntryStatus::MissingBefore | EntryStatus::MissingAfter
      )
    })
    .count();
  let new_missing = new_report
    .results
    .iter()
    .filter(|entry| filters.matches(&entry.name))
    .filter(|entry| {
      matches!(
        entry.status,
        EntryStatus::MissingBefore | EntryStatus::MissingAfter
      )
    })
    .count();

  DeltaTotals {
    entries,
    paired,
    missing_in_baseline,
    missing_in_new,
    baseline_errors,
    new_errors,
    baseline_missing,
    new_missing,
    ..DeltaTotals::default()
  }
}

fn read_report(path: &Path) -> Result<DiffReport, String> {
  let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
  serde_json::from_str(&raw).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

impl ReportMeta {
  fn from_report(report: &DiffReport, report_json: &Path, report_html: Option<&Path>) -> Self {
    Self {
      report_json: display_path(report_json),
      report_html: report_html.map(display_path),
      before_dir: report.before_dir.clone(),
      after_dir: report.after_dir.clone(),
      tolerance: report.tolerance,
      max_diff_percent: report.max_diff_percent,
      max_perceptual_distance: report.max_perceptual_distance,
      ignore_alpha: report.ignore_alpha,
      shard: report.shard.as_ref().map(|s| ReportShard {
        index: s.index,
        total: s.total,
      }),
    }
  }
}

fn diff_config(baseline: &DiffReport, new_report: &DiffReport) -> Vec<ConfigMismatch> {
  let mut mismatches = Vec::new();
  if baseline.tolerance != new_report.tolerance {
    mismatches.push(ConfigMismatch {
      field: "tolerance",
      baseline: baseline.tolerance.to_string(),
      new: new_report.tolerance.to_string(),
    });
  }
  if !float_eq(baseline.max_diff_percent, new_report.max_diff_percent) {
    mismatches.push(ConfigMismatch {
      field: "max_diff_percent",
      baseline: format!("{:.8}", baseline.max_diff_percent),
      new: format!("{:.8}", new_report.max_diff_percent),
    });
  }
  if baseline.ignore_alpha != new_report.ignore_alpha {
    mismatches.push(ConfigMismatch {
      field: "ignore_alpha",
      baseline: baseline.ignore_alpha.to_string(),
      new: new_report.ignore_alpha.to_string(),
    });
  }
  match (
    baseline.max_perceptual_distance,
    new_report.max_perceptual_distance,
  ) {
    (None, None) => {}
    (Some(a), Some(b)) => {
      if !float_eq(a, b) {
        mismatches.push(ConfigMismatch {
          field: "max_perceptual_distance",
          baseline: format!("{a:.8}"),
          new: format!("{b:.8}"),
        });
      }
    }
    (a, b) => mismatches.push(ConfigMismatch {
      field: "max_perceptual_distance",
      baseline: a
        .map(|v| format!("{v:.8}"))
        .unwrap_or_else(|| "-".to_string()),
      new: b
        .map(|v| format!("{v:.8}"))
        .unwrap_or_else(|| "-".to_string()),
    }),
  }

  match (baseline.shard.as_ref(), new_report.shard.as_ref()) {
    (None, None) => {}
    (Some(a), Some(b)) if a.index == b.index && a.total == b.total => {}
    (a, b) => mismatches.push(ConfigMismatch {
      field: "shard",
      baseline: a
        .map(|s| format!("{}/{}", s.index, s.total))
        .unwrap_or_else(|| "-".to_string()),
      new: b
        .map(|s| format!("{}/{}", s.index, s.total))
        .unwrap_or_else(|| "-".to_string()),
    }),
  }

  mismatches
}

fn index_entries(entries: Vec<DiffReportEntry>) -> BTreeMap<String, DiffReportEntry> {
  let mut map = BTreeMap::new();
  for entry in entries {
    map.insert(entry.name.clone(), entry);
  }
  map
}

fn to_entry_summary(entry: &DiffReportEntry) -> EntrySummary {
  EntrySummary {
    status: entry.status,
    before: entry.before.clone(),
    after: entry.after.clone(),
    diff: entry.diff.clone(),
    metrics: entry.metrics,
    error: entry.error.clone(),
  }
}

fn classify_delta(
  baseline: Option<&DiffReportEntry>,
  new_entry: Option<&DiffReportEntry>,
) -> (Option<f64>, Option<f64>, DeltaClassification) {
  match (baseline, new_entry) {
    (None, None) => (None, None, DeltaClassification::Unchanged),
    (None, Some(_)) => (None, None, DeltaClassification::MissingInBaseline),
    (Some(_), None) => (None, None, DeltaClassification::MissingInNew),
    (Some(b), Some(n)) => {
      let baseline_metrics = b.metrics;
      let new_metrics = n.metrics;
      if let (Some(bm), Some(nm)) = (baseline_metrics, new_metrics) {
        let diff_delta = nm.diff_percentage - bm.diff_percentage;
        let perceptual_delta = nm.perceptual_distance - bm.perceptual_distance;
        (
          Some(diff_delta),
          Some(perceptual_delta),
          classify_metrics(diff_delta, perceptual_delta),
        )
      } else if baseline_metrics.is_some() && new_metrics.is_none() {
        (None, None, DeltaClassification::Regressed)
      } else if baseline_metrics.is_none() && new_metrics.is_some() {
        (None, None, DeltaClassification::Improved)
      } else {
        let base_weight = b.status.kind_weight();
        let new_weight = n.status.kind_weight();
        if base_weight == new_weight {
          (None, None, DeltaClassification::Unchanged)
        } else if new_weight < base_weight {
          (None, None, DeltaClassification::Improved)
        } else {
          (None, None, DeltaClassification::Regressed)
        }
      }
    }
  }
}

fn classify_metrics(diff_delta: f64, perceptual_delta: f64) -> DeltaClassification {
  if diff_delta.abs() <= METRIC_EPS {
    if perceptual_delta.abs() <= METRIC_EPS {
      DeltaClassification::Unchanged
    } else if perceptual_delta < 0.0 {
      DeltaClassification::Improved
    } else {
      DeltaClassification::Regressed
    }
  } else if diff_delta < 0.0 {
    DeltaClassification::Improved
  } else {
    DeltaClassification::Regressed
  }
}

fn collect_top_metric_deltas(entries: &[DeltaEntry], improvements: bool) -> Vec<DeltaRankedEntry> {
  let mut out = entries
    .iter()
    .filter_map(|entry| {
      let diff = entry.diff_percentage_delta?;
      let perceptual = entry.perceptual_distance_delta?;
      Some((entry.name.clone(), diff, perceptual))
    })
    .filter(|(_, diff, perceptual)| {
      if improvements {
        *diff < -METRIC_EPS || (*diff).abs() <= METRIC_EPS && *perceptual < -METRIC_EPS
      } else {
        *diff > METRIC_EPS || (*diff).abs() <= METRIC_EPS && *perceptual > METRIC_EPS
      }
    })
    .map(|(name, diff, perceptual)| DeltaRankedEntry {
      name,
      diff_percentage_delta: diff,
      perceptual_distance_delta: perceptual,
    })
    .collect::<Vec<_>>();

  out.sort_by(|a, b| {
    let primary = if improvements {
      a.diff_percentage_delta
        .partial_cmp(&b.diff_percentage_delta)
        .unwrap_or(std::cmp::Ordering::Equal)
    } else {
      b.diff_percentage_delta
        .partial_cmp(&a.diff_percentage_delta)
        .unwrap_or(std::cmp::Ordering::Equal)
    };
    if primary != std::cmp::Ordering::Equal {
      return primary;
    }

    let secondary = if improvements {
      a.perceptual_distance_delta
        .partial_cmp(&b.perceptual_distance_delta)
        .unwrap_or(std::cmp::Ordering::Equal)
    } else {
      b.perceptual_distance_delta
        .partial_cmp(&a.perceptual_distance_delta)
        .unwrap_or(std::cmp::Ordering::Equal)
    };
    if secondary != std::cmp::Ordering::Equal {
      return secondary;
    }

    a.name.cmp(&b.name)
  });

  out
}

fn is_failing_regression(entry: &DeltaEntry, threshold: f64) -> bool {
  match entry.classification {
    DeltaClassification::Regressed => {
      if let Some(delta) = entry.diff_percentage_delta {
        if delta > threshold + METRIC_EPS {
          return true;
        }
        if delta.abs() <= METRIC_EPS {
          return entry
            .perceptual_distance_delta
            .map(|d| d > METRIC_EPS)
            .unwrap_or(false);
        }
        false
      } else {
        true
      }
    }
    DeltaClassification::MissingInNew => true,
    _ => false,
  }
}

fn float_eq(a: f64, b: f64) -> bool {
  (a - b).abs() <= METRIC_EPS
}

fn write_json_report(report: &DeltaReport, path: &Path) -> Result<(), String> {
  ensure_parent_dir(path)?;
  let json = serde_json::to_string_pretty(report)
    .map_err(|e| format!("failed to serialize JSON report: {e}"))?;
  fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_html_report(
  report: &DeltaReport,
  baseline_report_json: &Path,
  new_report_json: &Path,
  path: &Path,
  baseline_report_html_override: Option<&Path>,
  new_report_html_override: Option<&Path>,
) -> Result<(), String> {
  ensure_parent_dir(path)?;

  let html_dir = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("."));
  let html_dir = fs::canonicalize(&html_dir).unwrap_or(html_dir);

  let mismatch_block = if report.config_mismatches.is_empty() {
    "".to_string()
  } else {
    let mut rows = String::new();
    for mismatch in &report.config_mismatches {
      rows.push_str(&format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
        escape_html(mismatch.field),
        escape_html(&mismatch.baseline),
        escape_html(&mismatch.new)
      ));
    }
    format!(
      r#"<h2>Config mismatch</h2>
<p class="warning">Baseline/new reports were generated with different diff settings. Deltas may not be comparable.</p>
<table>
  <thead><tr><th>Field</th><th>Baseline</th><th>New</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
    )
  };

  let mut summary = format!(
    "Paired: {} | Improved: {} | Regressed: {} | Unchanged: {} | Missing entries (missing in baseline/new): {}/{} | Errors (baseline/new): {}/{} | Missing files (baseline/new): {}/{}",
    report.totals.paired,
    report.totals.improved,
    report.totals.regressed,
    report.totals.unchanged,
    report.totals.missing_in_baseline,
    report.totals.missing_in_new,
    report.totals.baseline_errors,
    report.totals.new_errors,
    report.totals.baseline_missing,
    report.totals.new_missing
  );
  if report
    .gating
    .as_ref()
    .map(|g| g.fail_on_regression)
    .unwrap_or(false)
  {
    let failing = report
      .results
      .iter()
      .filter(|entry| entry.failing_regression)
      .count();
    summary.push_str(&format!(" | Failing regressions: {failing}"));
  }
  let filters = format_filters_html(report.filters.as_ref());
  let gating = format_gating_html(report.gating.as_ref());

  let aggregate_block = format_aggregate_block(&report.aggregate);

  let top_improvements = format_top_list("Top improvements", &report.top_improvements, true);
  let top_regressions = format_top_list("Top regressions", &report.top_regressions, false);
  let failing_regressions = format_failing_regressions_block(report);

  let baseline_report_html = baseline_report_html_override
    .map(PathBuf::from)
    .or_else(|| guess_report_html_path(baseline_report_json));
  let new_report_html = new_report_html_override
    .map(PathBuf::from)
    .or_else(|| guess_report_html_path(new_report_json));

  // The diff report's `before`/`after`/`diff` paths are emitted relative to the directory
  // containing the report HTML, so prefer that directory when we can find the HTML file.
  let baseline_report_dir = baseline_report_html
    .as_ref()
    .and_then(|path| path.parent())
    .filter(|p| !p.as_os_str().is_empty())
    .or_else(|| {
      baseline_report_json
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    })
    .unwrap_or_else(|| Path::new("."));
  let baseline_report_dir = fs::canonicalize(baseline_report_dir).unwrap_or_else(|_| {
    if baseline_report_dir.as_os_str().is_empty() {
      PathBuf::from(".")
    } else {
      baseline_report_dir.to_path_buf()
    }
  });

  let new_report_dir = new_report_html
    .as_ref()
    .and_then(|path| path.parent())
    .filter(|p| !p.as_os_str().is_empty())
    .or_else(|| {
      new_report_json
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    })
    .unwrap_or_else(|| Path::new("."));
  let new_report_dir = fs::canonicalize(new_report_dir).unwrap_or_else(|_| {
    if new_report_dir.as_os_str().is_empty() {
      PathBuf::from(".")
    } else {
      new_report_dir.to_path_buf()
    }
  });

  let baseline_html_link = baseline_report_html
    .as_deref()
    .map(|path| path_for_report(&html_dir, path))
    .unwrap_or_else(|| "-".to_string());
  let new_html_link = new_report_html
    .as_deref()
    .map(|path| path_for_report(&html_dir, path))
    .unwrap_or_else(|| "-".to_string());

  let baseline_json_link = path_for_report(&html_dir, baseline_report_json);
  let new_json_link = path_for_report(&html_dir, new_report_json);

  let mut rows = String::new();
  for entry in &report.results {
    let anchor_id = entry_anchor_id(&entry.name);
    let baseline_status = entry
      .baseline
      .as_ref()
      .map(|s| s.status.label())
      .unwrap_or("-");
    let new_status = entry.new.as_ref().map(|s| s.status.label()).unwrap_or("-");
    let baseline_status_cell =
      if entry.baseline.is_some() && baseline_html_link != "-" {
        let href = format!("{baseline_html_link}#{anchor_id}");
        format!(
          r#"<a href="{href}">{status}</a>"#,
          href = escape_html(&href),
          status = escape_html(baseline_status)
        )
      } else {
        escape_html(baseline_status)
      };
    let new_status_cell = if entry.new.is_some() && new_html_link != "-" {
      let href = format!("{new_html_link}#{anchor_id}");
      format!(
        r#"<a href="{href}">{status}</a>"#,
        href = escape_html(&href),
        status = escape_html(new_status)
      )
    } else {
      escape_html(new_status)
    };

    let baseline_after_cell = entry
      .baseline
      .as_ref()
      .and_then(|s| s.after.as_deref())
      .map(|after| format_report_image_cell(&html_dir, &baseline_report_dir, "After", after))
      .unwrap_or_else(|| "-".to_string());
    let baseline_after_and_diff =
      if let Some(diff) = entry.baseline.as_ref().and_then(|s| s.diff.as_deref()) {
        format!(
          "{after}{diff}",
          after = baseline_after_cell,
          diff = format_report_image_cell(&html_dir, &baseline_report_dir, "Diff", diff)
        )
      } else {
        baseline_after_cell
      };

    let new_after_cell = entry
      .new
      .as_ref()
      .and_then(|s| s.after.as_deref())
      .map(|after| format_report_image_cell(&html_dir, &new_report_dir, "After", after))
      .unwrap_or_else(|| "-".to_string());
    let new_after_and_diff = if let Some(diff) = entry.new.as_ref().and_then(|s| s.diff.as_deref())
    {
      format!(
        "{after}{diff}",
        after = new_after_cell,
        diff = format_report_image_cell(&html_dir, &new_report_dir, "Diff", diff)
      )
    } else {
      new_after_cell
    };

    let baseline_diff = entry
      .baseline
      .as_ref()
      .and_then(|s| s.metrics)
      .map(format_diff_percentage_cell)
      .unwrap_or_else(|| "-".to_string());
    let new_diff = entry
      .new
      .as_ref()
      .and_then(|s| s.metrics)
      .map(format_diff_percentage_cell)
      .unwrap_or_else(|| "-".to_string());

    let baseline_perceptual = entry
      .baseline
      .as_ref()
      .and_then(|s| s.metrics)
      .map(|m| format!("{:.4}", m.perceptual_distance))
      .unwrap_or_else(|| "-".to_string());
    let new_perceptual = entry
      .new
      .as_ref()
      .and_then(|s| s.metrics)
      .map(|m| format!("{:.4}", m.perceptual_distance))
      .unwrap_or_else(|| "-".to_string());

    let diff_delta = entry
      .diff_percentage_delta
      .map(|d| format!("{:+.4}%", d))
      .unwrap_or_else(|| "-".to_string());
    let perceptual_delta = entry
      .perceptual_distance_delta
      .map(|d| format!("{:+.4}", d))
      .unwrap_or_else(|| "-".to_string());

    let baseline_error = entry
      .baseline
      .as_ref()
      .and_then(|s| s.error.as_deref())
      .unwrap_or("");
    let new_error = entry
      .new
      .as_ref()
      .and_then(|s| s.error.as_deref())
      .unwrap_or("");
    let error_combined = if baseline_error.is_empty() && new_error.is_empty() {
      "".to_string()
    } else if baseline_error.is_empty() {
      format!("new: {new_error}")
    } else if new_error.is_empty() {
      format!("baseline: {baseline_error}")
    } else {
      format!("baseline: {baseline_error}\nnew: {new_error}")
    };

    let row_class = {
      let row_class = entry.classification.row_class();
      let label_class = entry.classification.label();
      let mut classes = if row_class == label_class {
        row_class.to_string()
      } else {
        format!("{row_class} {label_class}")
      };
      if entry.failing_regression {
        classes.push_str(" failing");
      }
        classes
      };

    rows.push_str(&format!(
       "<tr id=\"{anchor_id}\" class=\"{row_class}\"><td><a href=\"#{anchor_id}\">{name}</a></td><td>{classification}</td><td>{baseline_status}</td><td>{baseline_diff}</td><td>{baseline_perceptual}</td><td>{baseline_after_and_diff}</td><td>{new_status}</td><td>{new_diff}</td><td>{new_perceptual}</td><td>{new_after_and_diff}</td><td>{diff_delta}</td><td>{perceptual_delta}</td><td class=\"error\">{error}</td></tr>",
       anchor_id = escape_html(&anchor_id),
       row_class = escape_html(&row_class),
       name = escape_html(&entry.name),
       classification = escape_html(entry.classification.label()),
       baseline_status = baseline_status_cell,
       baseline_diff = baseline_diff,
       baseline_perceptual = baseline_perceptual,
       baseline_after_and_diff = baseline_after_and_diff,
       new_status = new_status_cell,
       new_diff = new_diff,
       new_perceptual = new_perceptual,
       new_after_and_diff = new_after_and_diff,
       diff_delta = diff_delta,
       perceptual_delta = perceptual_delta,
       error = escape_html(&error_combined),
     ));
  }

  let content = format!(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Diff report delta</title>
    <style>
      body {{ font-family: sans-serif; margin: 20px; }}
      table {{ border-collapse: collapse; width: 100%; }}
      th, td {{ border: 1px solid #ddd; padding: 6px; vertical-align: top; }}
      th {{ background: #f3f3f3; position: sticky; top: 0; }}
      tr.improved {{ background: #f3fff3; }}
      tr.regressed {{ background: #fff3f3; }}
      tr.missing {{ background: #fffbe8; }}
      tr.missing-in-new {{ background: #fff3f3; }}
      tr.unchanged {{ background: #ffffff; }}
      tr.failing {{ outline: 2px solid #b00020; }}
      tr.failing td:first-child {{ box-shadow: inset 4px 0 0 #b00020; }}
      tr:target {{ outline: 3px solid #0066cc; }}
      tr[id] {{ scroll-margin-top: 50px; }}
      #all-entries-controls {{ margin-top: 16px; }}
      #all-entries-controls input[type="checkbox"] {{ position: absolute; left: -10000px; }}
      .entry-filters {{ margin-bottom: 10px; }}
      .entry-filters label {{ display: inline-block; margin-right: 12px; }}
      .entry-filters label:hover {{ text-decoration: underline; cursor: pointer; }}
      #show-improved:not(:checked) ~ .entry-filters label[for="show-improved"],
      #show-regressed:not(:checked) ~ .entry-filters label[for="show-regressed"],
      #show-missing-in-new:not(:checked) ~ .entry-filters label[for="show-missing-in-new"],
      #show-missing-in-baseline:not(:checked) ~ .entry-filters label[for="show-missing-in-baseline"],
      #show-unchanged:not(:checked) ~ .entry-filters label[for="show-unchanged"],
      #show-thumbnails:not(:checked) ~ .entry-filters label[for="show-thumbnails"] {{
        opacity: 0.5;
      }}
      #show-improved:not(:checked) ~ #all-entries tbody tr.improved {{ display: none; }}
      #show-regressed:not(:checked) ~ #all-entries tbody tr.regressed {{ display: none; }}
      #show-missing-in-new:not(:checked) ~ #all-entries tbody tr.missing-in-new {{ display: none; }}
      #show-missing-in-baseline:not(:checked) ~ #all-entries tbody tr.missing-in-baseline {{ display: none; }}
      #show-unchanged:not(:checked) ~ #all-entries tbody tr.unchanged {{ display: none; }}
      #show-only-failing:checked ~ .entry-filters label[for="show-only-failing"] {{ font-weight: bold; }}
      #show-only-failing:checked ~ #all-entries tbody tr:not(.failing) {{ display: none; }}
      #show-thumbnails:not(:checked) ~ #all-entries .thumb br {{ display: none; }}
      #show-thumbnails:not(:checked) ~ #all-entries .thumb a:nth-of-type(2) {{ display: none; }}
      .warning {{ color: #b00020; }}
      .error {{ color: #b00020; white-space: pre-wrap; }}
      .top-list table {{ width: auto; }}
      .thumb {{ display: inline-block; margin-right: 6px; }}
      .thumb img {{ max-width: 240px; max-height: 180px; display: block; }}
    </style>
  </head>
  <body>
    <h1>Diff report delta</h1>
    <p><strong>Baseline:</strong> {baseline_before} → {baseline_after}</p>
    <p><strong>Baseline report:</strong> {baseline_report_link}</p>
    <p><strong>Baseline report JSON:</strong> {baseline_report_json_link}</p>
    <p><strong>New:</strong> {new_before} → {new_after}</p>
    <p><strong>New report:</strong> {new_report_link}</p>
    <p><strong>New report JSON:</strong> {new_report_json_link}</p>
    <p><strong>Config:</strong> tolerance={tolerance}, max_diff_percent={max_diff_percent:.4}, max_perceptual_distance={max_perceptual}, ignore_alpha={ignore_alpha}, shard={shard}</p>
    <p><strong>Filters:</strong> {filters}</p>
    <p><strong>Gating:</strong> {gating}</p>
    <p><strong>Summary:</strong> {summary}</p>
    {aggregate_block}
    {mismatch_block}
    <div class="top-list">
      {failing_regressions}
      {top_regressions}
      {top_improvements}
    </div>
    <h2>All entries</h2>
    <div id="all-entries-controls">
      <input type="checkbox" id="show-improved" checked>
      <input type="checkbox" id="show-regressed" checked>
      <input type="checkbox" id="show-missing-in-new" checked>
      <input type="checkbox" id="show-missing-in-baseline" checked>
      <input type="checkbox" id="show-unchanged" checked>
      {failing_only_checkbox}
      <input type="checkbox" id="show-thumbnails" checked>
      <div class="entry-filters">
        <strong>Show:</strong>
        <label for="show-improved">Improved ({improved})</label>
        <label for="show-regressed">Regressed ({regressed})</label>
        <label for="show-missing-in-new">Missing in new ({missing_in_new})</label>
        <label for="show-missing-in-baseline">Missing in baseline ({missing_in_baseline})</label>
        <label for="show-unchanged">Unchanged ({unchanged})</label>
        {failing_only_label}
        <label for="show-thumbnails">Thumbnails</label>
      </div>
      <table id="all-entries">
      <thead>
        <tr>
          <th>Name</th>
          <th>Delta</th>
          <th>Baseline status</th>
          <th>Baseline diff %</th>
          <th>Baseline perceptual</th>
          <th>Baseline after | diff</th>
          <th>New status</th>
          <th>New diff %</th>
          <th>New perceptual</th>
          <th>New after | diff</th>
          <th>Δ diff %</th>
          <th>Δ perceptual</th>
          <th>Error</th>
        </tr>
      </thead>
      <tbody>
        {rows}
      </tbody>
    </table>
    </div>
  </body>
</html>
"#,
    baseline_before = escape_html(&report.baseline.before_dir),
    baseline_after = escape_html(&report.baseline.after_dir),
    new_before = escape_html(&report.new.before_dir),
    new_after = escape_html(&report.new.after_dir),
    baseline_report_link = format_report_link(&baseline_html_link),
    new_report_link = format_report_link(&new_html_link),
    baseline_report_json_link = format_report_link(&baseline_json_link),
    new_report_json_link = format_report_link(&new_json_link),
    tolerance = report.new.tolerance,
    max_diff_percent = report.new.max_diff_percent,
    max_perceptual = report
      .new
      .max_perceptual_distance
      .map(|d| format!("{d:.4}"))
      .unwrap_or_else(|| "-".to_string()),
    ignore_alpha = if report.new.ignore_alpha { "yes" } else { "no" },
    shard = shard_label(&report.new.shard),
    filters = filters,
    gating = gating,
    summary = escape_html(&summary),
    aggregate_block = aggregate_block,
    mismatch_block = mismatch_block,
    failing_regressions = failing_regressions,
    top_regressions = top_regressions,
    top_improvements = top_improvements,
    improved = report.totals.improved,
    regressed = report.totals.regressed,
    unchanged = report.totals.unchanged,
    missing_in_new = report.totals.missing_in_new,
    missing_in_baseline = report.totals.missing_in_baseline,
    failing_only_checkbox = if report
      .gating
      .as_ref()
      .map(|g| g.fail_on_regression)
      .unwrap_or(false)
    {
      r#"<input type="checkbox" id="show-only-failing">"#
    } else {
      ""
    },
    failing_only_label = if report
      .gating
      .as_ref()
      .map(|g| g.fail_on_regression)
      .unwrap_or(false)
    {
      let failing = report
        .results
        .iter()
        .filter(|entry| entry.failing_regression)
        .count();
      format!(r#"<label for="show-only-failing">Only failing ({failing})</label>"#)
    } else {
      String::new()
    },
    rows = rows,
  );

  fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn format_report_image_cell(
  delta_html_dir: &Path,
  report_dir: &Path,
  label: &str,
  report_relative_path: &str,
) -> String {
  let target_path = resolve_report_path(report_dir, report_relative_path);
  let rel = path_for_report(delta_html_dir, &target_path);
  format_linked_image(label, &rel)
}

fn format_filters_html(filters: Option<&ReportFilters>) -> String {
  let Some(filters) = filters else {
    return "-".to_string();
  };

  let mut parts = Vec::new();
  if !filters.include.is_empty() {
    let patterns = filters
      .include
      .iter()
      .map(|pattern| format!("<code>{}</code>", escape_html(pattern)))
      .collect::<Vec<_>>()
      .join(", ");
    parts.push(format!("include=[{patterns}]"));
  }
  if !filters.exclude.is_empty() {
    let patterns = filters
      .exclude
      .iter()
      .map(|pattern| format!("<code>{}</code>", escape_html(pattern)))
      .collect::<Vec<_>>()
      .join(", ");
    parts.push(format!("exclude=[{patterns}]"));
  }

  if parts.is_empty() {
    "-".to_string()
  } else {
    parts.push(format!(
      "matched={}/{}",
      filters.matched_entries, filters.total_entries
    ));
    parts.join(" ")
  }
}

fn format_gating_html(gating: Option<&ReportGating>) -> String {
  let Some(gating) = gating else {
    return "-".to_string();
  };

  let enabled = if gating.fail_on_regression {
    "yes"
  } else {
    "no"
  };
  format!(
    "fail_on_regression={enabled}, threshold=<code>{:.4}%</code>",
    gating.regression_threshold_percent
  )
}

fn format_diff_percentage_cell(metrics: MetricsSummary) -> String {
  if metrics.total_pixels > 0 {
    let title = format!(
      "{}/{} pixels differ",
      metrics.pixel_diff, metrics.total_pixels
    );
    format!(
      r#"<span title="{title}">{percent:.4}%</span>"#,
      title = escape_html(&title),
      percent = metrics.diff_percentage
    )
  } else {
    format!("{:.4}%", metrics.diff_percentage)
  }
}

fn resolve_report_path(report_dir: &Path, report_relative_path: &str) -> PathBuf {
  let raw = PathBuf::from(report_relative_path);
  if raw.is_absolute() {
    raw
  } else {
    report_dir.join(raw)
  }
}

fn format_report_link(href: &str) -> String {
  if href == "-" {
    return "-".to_string();
  }
  let escaped = escape_html(href);
  format!(r#"<a href="{p}">{p}</a>"#, p = escaped)
}

fn guess_report_html_path(json_path: &Path) -> Option<PathBuf> {
  let dir = json_path.parent()?;
  let stem = json_path.file_stem()?.to_str()?;

  let stem_candidate = dir.join(format!("{stem}.html"));
  if stem_candidate.is_file() {
    return Some(stem_candidate);
  }

  let report_candidate = dir.join("report.html");
  if report_candidate.is_file() {
    return Some(report_candidate);
  }

  let diff_candidate = dir.join("diff_report.html");
  if diff_candidate.is_file() {
    return Some(diff_candidate);
  }

  None
}

fn format_aggregate_block(metrics: &AggregateMetrics) -> String {
  if metrics.paired_with_metrics == 0 {
    return "<p><strong>Aggregate:</strong> -</p>".to_string();
  }

  let weighted_delta = metrics
    .delta
    .weighted_diff_percentage
    .map(|v| format!("{:+.4}%", v))
    .unwrap_or_else(|| "-".to_string());
  let mean_delta = metrics
    .delta
    .mean_diff_percentage
    .map(|v| format!("{:+.4}%", v))
    .unwrap_or_else(|| "-".to_string());
  let perceptual_delta = metrics
    .delta
    .mean_perceptual_distance
    .map(|v| format!("{:+.4}", v))
    .unwrap_or_else(|| "-".to_string());

  let baseline_weighted = metrics
    .baseline
    .weighted_diff_percentage
    .map(|v| format!("{v:.4}%"))
    .unwrap_or_else(|| "-".to_string());
  let new_weighted = metrics
    .new
    .weighted_diff_percentage
    .map(|v| format!("{v:.4}%"))
    .unwrap_or_else(|| "-".to_string());

  let baseline_mean = metrics
    .baseline
    .mean_diff_percentage
    .map(|v| format!("{v:.4}%"))
    .unwrap_or_else(|| "-".to_string());
  let new_mean = metrics
    .new
    .mean_diff_percentage
    .map(|v| format!("{v:.4}%"))
    .unwrap_or_else(|| "-".to_string());

  let baseline_perceptual = metrics
    .baseline
    .mean_perceptual_distance
    .map(|v| format!("{v:.4}"))
    .unwrap_or_else(|| "-".to_string());
  let new_perceptual = metrics
    .new
    .mean_perceptual_distance
    .map(|v| format!("{v:.4}"))
    .unwrap_or_else(|| "-".to_string());

  format!(
    r#"<h2>Aggregate</h2>
<p>Computed over {paired} paired entries with metrics.</p>
<table>
  <thead>
    <tr><th>Metric</th><th>Baseline</th><th>New</th><th>Δ</th></tr>
  </thead>
  <tbody>
    <tr><td>Weighted diff %</td><td>{baseline_weighted}</td><td>{new_weighted}</td><td>{weighted_delta}</td></tr>
    <tr><td>Mean diff %</td><td>{baseline_mean}</td><td>{new_mean}</td><td>{mean_delta}</td></tr>
    <tr><td>Mean perceptual</td><td>{baseline_perceptual}</td><td>{new_perceptual}</td><td>{perceptual_delta}</td></tr>
  </tbody>
</table>"#,
    paired = metrics.paired_with_metrics,
    baseline_weighted = baseline_weighted,
    new_weighted = new_weighted,
    weighted_delta = weighted_delta,
    baseline_mean = baseline_mean,
    new_mean = new_mean,
    mean_delta = mean_delta,
    baseline_perceptual = baseline_perceptual,
    new_perceptual = new_perceptual,
    perceptual_delta = perceptual_delta,
  )
}

fn format_top_list(title: &str, entries: &[DeltaRankedEntry], improvements: bool) -> String {
  if entries.is_empty() {
    return format!("<h2>{}</h2><p>-</p>", escape_html(title));
  }

  let mut rows = String::new();
  for entry in entries {
    let anchor_id = entry_anchor_id(&entry.name);
    rows.push_str(&format!(
      "<tr><td><a href=\"#{anchor_id}\">{name}</a></td><td>{diff:+.4}%</td><td>{perceptual:+.4}</td></tr>",
      anchor_id = escape_html(&anchor_id),
      name = escape_html(&entry.name),
      diff = entry.diff_percentage_delta,
      perceptual = entry.perceptual_distance_delta,
    ));
  }

  let note = if improvements {
    "more negative is better"
  } else {
    "more positive is worse"
  };

  format!(
    r#"<h2>{title}</h2>
<p><em>{note}</em></p>
<table>
  <thead><tr><th>Name</th><th>Δ diff %</th><th>Δ perceptual</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
    title = escape_html(title),
    note = escape_html(note),
    rows = rows
  )
}

fn format_failing_regressions_block(report: &DeltaReport) -> String {
  if !report
    .gating
    .as_ref()
    .map(|g| g.fail_on_regression)
    .unwrap_or(false)
  {
    return "".to_string();
  }

  let failing = report
    .results
    .iter()
    .filter(|entry| entry.failing_regression)
    .collect::<Vec<_>>();
  if failing.is_empty() {
    return "<h2>Failing regressions</h2><p>-</p>".to_string();
  }

  let mut rows = String::new();
  for entry in failing {
    let anchor_id = entry_anchor_id(&entry.name);
    let diff = entry
      .diff_percentage_delta
      .map(|d| format!("{:+.4}%", d))
      .unwrap_or_else(|| "-".to_string());
    let perceptual = entry
      .perceptual_distance_delta
      .map(|d| format!("{:+.4}", d))
      .unwrap_or_else(|| "-".to_string());
    rows.push_str(&format!(
      "<tr><td><a href=\"#{anchor_id}\">{name}</a></td><td>{classification}</td><td>{diff}</td><td>{perceptual}</td></tr>",
      anchor_id = escape_html(&anchor_id),
      name = escape_html(&entry.name),
      classification = escape_html(entry.classification.label()),
      diff = escape_html(&diff),
      perceptual = escape_html(&perceptual),
    ));
  }

  format!(
    r#"<h2>Failing regressions</h2>
<table>
  <thead><tr><th>Name</th><th>Delta</th><th>Δ diff %</th><th>Δ perceptual</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
    rows = rows
  )
}
