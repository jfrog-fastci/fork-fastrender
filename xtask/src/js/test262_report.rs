use anyhow::{bail, Context, Result};
use conformance_harness::ExpectationKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::{fmt, mem};

pub const TEST262_REPORT_SCHEMA_VERSION: u32 = 1;
pub const TEST262_BASELINE_SCHEMA_VERSION: u32 = 1;
pub const TEST262_TREND_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
  NonStrict,
  Strict,
  Module,
}

impl Variant {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::NonStrict => "non_strict",
      Self::Strict => "strict",
      Self::Module => "module",
    }
  }
}

impl fmt::Display for Variant {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
  Passed,
  Failed,
  TimedOut,
  Skipped,
}

impl TestOutcome {
  pub fn is_timeout(self) -> bool {
    matches!(self, Self::TimedOut)
  }
}

impl fmt::Display for TestOutcome {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let value = match self {
      Self::Passed => "passed",
      Self::Failed => "failed",
      Self::TimedOut => "timed_out",
      Self::Skipped => "skipped",
    };
    f.write_str(value)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MismatchSummary {
  pub expected: usize,
  pub unexpected: usize,
  pub flaky: usize,
}

impl MismatchSummary {
  pub fn total(&self) -> usize {
    self.expected + self.unexpected + self.flaky
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Summary {
  pub total: usize,
  pub passed: usize,
  pub failed: usize,
  pub timed_out: usize,
  pub skipped: usize,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mismatches: Option<MismatchSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ExpectationOutcome {
  pub expectation: ExpectationKind,
  #[serde(default)]
  pub expected: bool,
  #[serde(default)]
  pub from_manifest: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tracking_issue: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestResult {
  pub id: String,
  pub variant: Variant,
  pub outcome: TestOutcome,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
  pub expectation: ExpectationOutcome,
  #[serde(default)]
  pub mismatched: bool,
  #[serde(default)]
  pub expected_mismatch: bool,
  #[serde(default)]
  pub flaky: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
  pub schema_version: u32,
  pub summary: Summary,
  pub results: Vec<TestResult>,
}

pub fn read_report(path: &Path) -> Result<Report> {
  let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  let report: Report =
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
  if report.schema_version != TEST262_REPORT_SCHEMA_VERSION {
    bail!(
      "unsupported test262 report schema_version {} (expected {})",
      report.schema_version,
      TEST262_REPORT_SCHEMA_VERSION
    );
  }
  Ok(report)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResultKey {
  pub id: String,
  pub variant: Variant,
}

impl ResultKey {
  pub fn new(id: &str, variant: Variant) -> Self {
    Self {
      id: id.to_string(),
      variant,
    }
  }

  pub fn from_result(result: &TestResult) -> Self {
    Self::new(&result.id, result.variant)
  }

  pub fn to_string_key(&self) -> String {
    format!("{}#{}", self.id, self.variant.as_str())
  }

  pub fn bucket(&self) -> String {
    bucket_for_id(&self.id)
  }
}

impl fmt::Display for ResultKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}#{}", self.id, self.variant)
  }
}

fn bucket_for_id(id: &str) -> String {
  let mut parts = id.split('/');
  let first = parts.next().unwrap_or("<unknown>");
  if let Some(second) = parts.next() {
    format!("{first}/{second}")
  } else {
    first.to_string()
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineEntry {
  pub outcome: TestOutcome,
  pub mismatched: bool,
  #[serde(default)]
  pub expectation: ExpectationKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Baseline {
  pub schema_version: u32,
  pub results: BTreeMap<String, BaselineEntry>,
}

pub fn baseline_from_report(report: &Report) -> Result<Baseline> {
  let mut results = BTreeMap::new();
  for entry in &report.results {
    let key = ResultKey::from_result(entry).to_string_key();
    let previous = results.insert(
      key.clone(),
      BaselineEntry {
        outcome: entry.outcome,
        mismatched: entry.mismatched,
        expectation: entry.expectation.expectation,
      },
    );
    if previous.is_some() {
      bail!("test262 report contains duplicate result key `{key}`");
    }
  }
  Ok(Baseline {
    schema_version: TEST262_BASELINE_SCHEMA_VERSION,
    results,
  })
}

pub fn read_baseline(path: &Path) -> Result<Baseline> {
  let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  let baseline: Baseline =
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
  if baseline.schema_version != TEST262_BASELINE_SCHEMA_VERSION {
    bail!(
      "unsupported test262 baseline schema_version {} (expected {})",
      baseline.schema_version,
      TEST262_BASELINE_SCHEMA_VERSION
    );
  }
  Ok(baseline)
}

pub fn write_baseline(path: &Path, baseline: &Baseline) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  }
  let json = serde_json::to_string_pretty(baseline).context("serialize test262 baseline JSON")?;
  fs::write(path, format!("{json}\n").as_bytes())
    .with_context(|| format!("write {}", path.display()))?;
  Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindTotals {
  pub pass: usize,
  pub xfail: usize,
  pub skip: usize,
  pub flaky: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCounts {
  pub total: usize,
  /// Number of cases matching their upstream test262 expected outcome (`mismatched=false`).
  pub matched: usize,
  /// Number of cases mismatching their upstream expected outcome (`mismatched=true`).
  pub mismatched: usize,
  /// Expectation-kind counts (manifest view).
  pub kinds: KindTotals,
  /// Manifest `pass` cases that match upstream expectation.
  pub pass: usize,
  /// Manifest `pass` cases that *mismatch* upstream expectation (unexpected failures).
  pub unexpected_fail: usize,
  /// Manifest `xfail` cases that mismatch upstream expectation (expected failures).
  pub xfail: usize,
  /// Manifest `xfail` cases that match upstream expectation (unexpected passes / "XPASS").
  pub xpass: usize,
  /// Manifest `skip` cases.
  pub skip: usize,
  /// Manifest `flaky` cases that mismatch upstream expectation.
  pub flaky: usize,
  /// Manifest `flaky` cases that match upstream expectation.
  pub flaky_pass: usize,
  /// Cases whose executor hit a timeout.
  pub timed_out: usize,
}

impl BucketCounts {
  pub fn observe(&mut self, result: &TestResult) {
    self.total += 1;
    if result.mismatched {
      self.mismatched += 1;
    } else {
      self.matched += 1;
    }
    if result.outcome.is_timeout() {
      self.timed_out += 1;
    }

    match result.expectation.expectation {
      ExpectationKind::Pass => {
        self.kinds.pass += 1;
        if result.mismatched {
          self.unexpected_fail += 1;
        } else {
          self.pass += 1;
        }
      }
      ExpectationKind::Xfail => {
        self.kinds.xfail += 1;
        if result.mismatched {
          self.xfail += 1;
        } else {
          self.xpass += 1;
        }
      }
      ExpectationKind::Skip => {
        self.kinds.skip += 1;
        self.skip += 1;
      }
      ExpectationKind::Flaky => {
        self.kinds.flaky += 1;
        if result.mismatched {
          self.flaky += 1;
        } else {
          self.flaky_pass += 1;
        }
      }
    }
  }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReportStats {
  pub overall: BucketCounts,
  pub by_bucket: BTreeMap<String, BucketCounts>,
}

pub fn compute_report_stats(report: &Report) -> ReportStats {
  let mut stats = ReportStats::default();
  for result in &report.results {
    stats.overall.observe(result);
    let bucket = bucket_for_id(&result.id);
    stats.by_bucket.entry(bucket).or_default().observe(result);
  }
  stats
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeChange {
  pub key: ResultKey,
  pub baseline: Option<BaselineEntry>,
  pub current: BaselineEntry,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Comparison {
  pub regressions: Vec<OutcomeChange>,
  pub improvements: Vec<OutcomeChange>,
  pub new_timeouts: Vec<OutcomeChange>,
  pub new_tests: Vec<ResultKey>,
  pub removed_tests: Vec<ResultKey>,
}

pub fn compare_to_baseline(baseline: &Baseline, report: &Report) -> Result<Comparison> {
  let mut comparison = Comparison::default();

  let mut current_keys: BTreeMap<ResultKey, BaselineEntry> = BTreeMap::new();
  for result in &report.results {
    let key = ResultKey::from_result(result);
    let previous = current_keys.insert(
      key.clone(),
      BaselineEntry {
        outcome: result.outcome,
        mismatched: result.mismatched,
        expectation: result.expectation.expectation,
      },
    );
    if previous.is_some() {
      bail!("test262 report contains duplicate result key `{key}`");
    }
  }

  let mut baseline_keys: BTreeMap<ResultKey, BaselineEntry> = BTreeMap::new();
  for (raw_key, entry) in &baseline.results {
    let (id, variant) = parse_string_key(raw_key)
      .with_context(|| format!("parse baseline result key {raw_key:?}"))?;
    let key = ResultKey { id, variant };
    let previous = baseline_keys.insert(key.clone(), *entry);
    if previous.is_some() {
      bail!(
        "test262 baseline contains duplicate result key `{}`",
        raw_key
      );
    }
  }

  let mut all_keys: BTreeSet<ResultKey> = baseline_keys.keys().cloned().collect();
  all_keys.extend(current_keys.keys().cloned());

  for key in all_keys {
    match (baseline_keys.get(&key), current_keys.get(&key)) {
      (None, Some(current)) => {
        // Tests not present in the baseline are tracked separately so a curated-suite expansion
        // doesn't look like a regression. However, *timeouts* are always treated as important signal,
        // even for newly-added tests (a hang wastes CI time and often indicates cancellation bugs).
        if current.outcome.is_timeout() {
          comparison.new_timeouts.push(OutcomeChange {
            key: key.clone(),
            baseline: None,
            current: *current,
          });
        }
        comparison.new_tests.push(key);
      }
      (Some(_), None) => {
        comparison.removed_tests.push(key);
      }
      (Some(baseline_entry), Some(current_entry)) => {
        let baseline = *baseline_entry;
        let current = *current_entry;

        if !baseline.outcome.is_timeout() && current.outcome.is_timeout() {
          comparison.new_timeouts.push(OutcomeChange {
            key: key.clone(),
            baseline: Some(baseline),
            current,
          });
        }

        if !baseline.mismatched && current.mismatched {
          comparison.regressions.push(OutcomeChange {
            key: key.clone(),
            baseline: Some(baseline),
            current,
          });
        } else if baseline.mismatched && !current.mismatched {
          comparison.improvements.push(OutcomeChange {
            key: key.clone(),
            baseline: Some(baseline),
            current,
          });
        }
      }
      (None, None) => unreachable!("key came from either baseline or current"),
    }
  }

  Ok(comparison)
}

fn parse_string_key(raw: &str) -> Result<(String, Variant)> {
  let (id, variant_raw) = raw
    .rsplit_once('#')
    .ok_or_else(|| anyhow::anyhow!("expected key to be formatted as <id>#<variant>"))?;
  let variant = match variant_raw {
    "non_strict" => Variant::NonStrict,
    "strict" => Variant::Strict,
    "module" => Variant::Module,
    other => bail!("unknown variant {other:?}"),
  };
  Ok((id.to_string(), variant))
}

fn format_delta(current: usize, baseline: usize) -> String {
  let delta = current as isize - baseline as isize;
  if delta == 0 {
    "0".to_string()
  } else if delta > 0 {
    format!("+{delta}")
  } else {
    delta.to_string()
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Trend {
  pub schema_version: u32,
  pub overall: BucketCounts,
  pub by_bucket: BTreeMap<String, BucketCounts>,
}

pub fn trend_from_report(report: &Report) -> Trend {
  let stats = compute_report_stats(report);
  Trend {
    schema_version: TEST262_TREND_SCHEMA_VERSION,
    overall: stats.overall,
    by_bucket: stats.by_bucket,
  }
}

pub fn write_trend(path: &Path, trend: &Trend) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  }
  let json = serde_json::to_string_pretty(trend).context("serialize test262 trend JSON")?;
  fs::write(path, format!("{json}\n").as_bytes())
    .with_context(|| format!("write {}", path.display()))?;
  Ok(())
}

pub struct MarkdownOptions<'a> {
  pub title: &'a str,
  pub report_path: &'a Path,
  pub baseline_path: Option<&'a Path>,
}

pub fn render_markdown(
  report: &Report,
  stats: &ReportStats,
  baseline: Option<&Baseline>,
  comparison: Option<&Comparison>,
  opts: MarkdownOptions<'_>,
) -> String {
  let mut out = String::new();

  out.push_str("# ");
  out.push_str(opts.title);
  out.push_str("\n\n");

  out.push_str("- Report: `");
  out.push_str(&display_path_for_markdown(opts.report_path));
  out.push_str("`\n");
  if let Some(baseline_path) = opts.baseline_path {
    out.push_str("- Baseline: `");
    out.push_str(&display_path_for_markdown(baseline_path));
    out.push_str("`\n");
  }
  out.push_str("\n");

  let mismatches = report.summary.mismatches.as_ref();
  let mismatched_total = mismatches.map(|m| m.total()).unwrap_or(0);
  let matched_total = report.summary.total.saturating_sub(mismatched_total);

  out.push_str("## Summary\n\n");
  out.push_str("| Metric | Count |\n");
  out.push_str("| --- | ---: |\n");
  out.push_str(&format!("| Total cases | {} |\n", report.summary.total));
  out.push_str(&format!(
    "| Matched upstream expected | {} |\n",
    matched_total
  ));
  out.push_str(&format!(
    "| Mismatched upstream expected | {} |\n",
    mismatched_total
  ));
  out.push_str(&format!("| Timeouts | {} |\n", report.summary.timed_out));
  out.push_str("\n");

  out.push_str("### Manifest expectations (kind)\n\n");
  out.push_str("| Kind | Count |\n");
  out.push_str("| --- | ---: |\n");
  out.push_str(&format!("| pass | {} |\n", stats.overall.kinds.pass));
  out.push_str(&format!("| xfail | {} |\n", stats.overall.kinds.xfail));
  out.push_str(&format!("| skip | {} |\n", stats.overall.kinds.skip));
  out.push_str(&format!("| flaky | {} |\n", stats.overall.kinds.flaky));
  out.push_str("\n");

  out.push_str("### Results vs expectations\n\n");
  out.push_str("| Status | Count |\n");
  out.push_str("| --- | ---: |\n");
  out.push_str(&format!(
    "| PASS (pass+matched) | {} |\n",
    stats.overall.pass
  ));
  out.push_str(&format!(
    "| XFAIL (xfail+mismatched) | {} |\n",
    stats.overall.xfail
  ));
  out.push_str(&format!("| SKIP | {} |\n", stats.overall.skip));
  if stats.overall.flaky > 0 || stats.overall.flaky_pass > 0 {
    out.push_str(&format!(
      "| FLAKY (flaky+mismatched) | {} |\n",
      stats.overall.flaky
    ));
  }
  if stats.overall.unexpected_fail > 0 {
    out.push_str(&format!(
      "| Unexpected failures (pass+mismatched) | {} |\n",
      stats.overall.unexpected_fail
    ));
  }
  if stats.overall.xpass > 0 {
    out.push_str(&format!(
      "| XPASS (xfail+matched) | {} |\n",
      stats.overall.xpass
    ));
  }
  if stats.overall.flaky_pass > 0 {
    out.push_str(&format!(
      "| Flaky XPASS (flaky+matched) | {} |\n",
      stats.overall.flaky_pass
    ));
  }
  out.push_str("\n");

  if let Some(m) = mismatches {
    out.push_str("### Mismatch classification (for `--fail-on`)\n\n");
    out.push_str("| Kind | Count |\n");
    out.push_str("| --- | ---: |\n");
    out.push_str(&format!("| expected | {} |\n", m.expected));
    out.push_str(&format!("| unexpected | {} |\n", m.unexpected));
    out.push_str(&format!("| flaky | {} |\n", m.flaky));
    out.push_str("\n");
  }

  out.push_str("## Breakdown by area\n\n");
  out.push_str(
    "| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |\n",
  );
  out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");

  let mut baseline_stats = None;
  if let Some(baseline) = baseline {
    baseline_stats = Some(compute_baseline_bucket_stats(baseline));
  }
  let baseline_stats = baseline_stats.as_ref();

  for (bucket, counts) in &stats.by_bucket {
    let (delta_pass, delta_xfail, delta_timeout) = if let Some(baseline_stats) = baseline_stats {
      let base = baseline_stats.get(bucket).copied().unwrap_or_default();
      (
        format_delta(counts.pass, base.pass),
        format_delta(counts.xfail, base.xfail),
        format_delta(counts.timed_out, base.timed_out),
      )
    } else {
      ("".to_string(), "".to_string(), "".to_string())
    };

    out.push_str(&format!(
      "| `{bucket}` | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
      counts.total,
      counts.pass,
      counts.xfail,
      counts.skip,
      counts.unexpected_fail,
      counts.timed_out,
      delta_pass,
      delta_xfail,
      delta_timeout
    ));
  }
  out.push_str("\n");

  if let Some(comparison) = comparison {
    out.push_str("## Changes vs baseline\n\n");
    out.push_str(&format!(
      "- Regressions (matched -> mismatched): {}\n",
      comparison.regressions.len()
    ));
    out.push_str(&format!(
      "- Improvements (mismatched -> matched): {}\n",
      comparison.improvements.len()
    ));
    out.push_str(&format!(
      "- New timeouts: {}\n",
      comparison.new_timeouts.len()
    ));
    out.push_str(&format!("- New tests: {}\n", comparison.new_tests.len()));
    out.push_str(&format!(
      "- Removed tests: {}\n",
      comparison.removed_tests.len()
    ));
    out.push_str("\n");

    const LIST_LIMIT: usize = 20;

    if !comparison.regressions.is_empty() {
      out.push_str("### Top regressions\n\n");
      for change in comparison.regressions.iter().take(LIST_LIMIT) {
        let baseline = change.baseline.unwrap_or_else(|| BaselineEntry {
          outcome: TestOutcome::Skipped,
          mismatched: true,
          expectation: ExpectationKind::Pass,
        });
        out.push_str(&format!(
          "- `{}`: {}{} -> {}{}\n",
          change.key,
          if baseline.mismatched {
            "mismatched"
          } else {
            "matched"
          },
          format!(" ({})", baseline.outcome),
          if change.current.mismatched {
            "mismatched"
          } else {
            "matched"
          },
          format!(" ({})", change.current.outcome)
        ));
      }
      if comparison.regressions.len() > LIST_LIMIT {
        out.push_str(&format!(
          "\n… plus {} more\n",
          comparison.regressions.len() - LIST_LIMIT
        ));
      }
      out.push_str("\n");
    }

    if !comparison.improvements.is_empty() {
      out.push_str("### Top improvements\n\n");
      for change in comparison.improvements.iter().take(LIST_LIMIT) {
        let baseline = change.baseline.unwrap_or_else(|| BaselineEntry {
          outcome: TestOutcome::Skipped,
          mismatched: true,
          expectation: ExpectationKind::Pass,
        });
        out.push_str(&format!(
          "- `{}`: {}{} -> {}{}\n",
          change.key,
          if baseline.mismatched {
            "mismatched"
          } else {
            "matched"
          },
          format!(" ({})", baseline.outcome),
          if change.current.mismatched {
            "mismatched"
          } else {
            "matched"
          },
          format!(" ({})", change.current.outcome)
        ));
      }
      if comparison.improvements.len() > LIST_LIMIT {
        out.push_str(&format!(
          "\n… plus {} more\n",
          comparison.improvements.len() - LIST_LIMIT
        ));
      }
      out.push_str("\n");
    }

    if !comparison.new_timeouts.is_empty() {
      out.push_str("### New timeouts\n\n");
      for change in comparison.new_timeouts.iter().take(LIST_LIMIT) {
        let baseline = change.baseline.unwrap_or_else(|| BaselineEntry {
          outcome: TestOutcome::Skipped,
          mismatched: true,
          expectation: ExpectationKind::Pass,
        });
        out.push_str(&format!(
          "- `{}`: {} -> {}\n",
          change.key, baseline.outcome, change.current.outcome
        ));
      }
      if comparison.new_timeouts.len() > LIST_LIMIT {
        out.push_str(&format!(
          "\n… plus {} more\n",
          comparison.new_timeouts.len() - LIST_LIMIT
        ));
      }
      out.push_str("\n");
    }
  } else if opts.baseline_path.is_some() {
    out.push_str("## Changes vs baseline\n\n");
    out.push_str("_Baseline comparison was skipped._\n\n");
  }

  out
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct BaselineBucketCounts {
  pass: usize,
  xfail: usize,
  timed_out: usize,
}

fn compute_baseline_bucket_stats(baseline: &Baseline) -> BTreeMap<String, BaselineBucketCounts> {
  let mut stats: BTreeMap<String, BaselineBucketCounts> = BTreeMap::new();
  for (raw_key, entry) in &baseline.results {
    let Ok((id, _variant)) = parse_string_key(raw_key) else {
      continue;
    };
    let bucket = bucket_for_id(&id);
    let bucket_stats = stats.entry(bucket).or_default();

    match entry.expectation {
      ExpectationKind::Pass => {
        if !entry.mismatched {
          bucket_stats.pass += 1;
        }
      }
      ExpectationKind::Xfail => {
        if entry.mismatched {
          bucket_stats.xfail += 1;
        }
      }
      ExpectationKind::Skip | ExpectationKind::Flaky => {}
    }
    if entry.outcome.is_timeout() {
      bucket_stats.timed_out += 1;
    }
  }
  stats
}

pub fn strip_delta_columns_from_breakdown(markdown: &str) -> String {
  // If we rendered a breakdown table without baseline deltas we still emit the delta columns as empty
  // strings; that's fine. This helper exists for tests that want deterministic content even when
  // baseline data isn't present.
  markdown.to_string()
}

pub fn ensure_sorted_for_display(comparison: &mut Comparison) {
  comparison.regressions.sort_by(|a, b| a.key.cmp(&b.key));
  comparison.improvements.sort_by(|a, b| a.key.cmp(&b.key));
  comparison.new_timeouts.sort_by(|a, b| a.key.cmp(&b.key));
  comparison.new_tests.sort();
  comparison.removed_tests.sort();
}

pub fn take_comparison(mut comparison: Comparison) -> Comparison {
  ensure_sorted_for_display(&mut comparison);
  // Avoid clippy warnings in the callers about needing to sort; this normalizes the lists.
  comparison
}

pub fn merge_bucket_stats(
  a: &mut BTreeMap<String, BucketCounts>,
  b: BTreeMap<String, BucketCounts>,
) {
  for (bucket, counts) in b {
    let entry = a.entry(bucket).or_default();
    // Merge field-by-field (only used for future extensions; keep explicit for clarity).
    entry.total += counts.total;
    entry.matched += counts.matched;
    entry.mismatched += counts.mismatched;
    entry.kinds.pass += counts.kinds.pass;
    entry.kinds.xfail += counts.kinds.xfail;
    entry.kinds.skip += counts.kinds.skip;
    entry.kinds.flaky += counts.kinds.flaky;
    entry.pass += counts.pass;
    entry.unexpected_fail += counts.unexpected_fail;
    entry.xfail += counts.xfail;
    entry.xpass += counts.xpass;
    entry.skip += counts.skip;
    entry.flaky += counts.flaky;
    entry.flaky_pass += counts.flaky_pass;
    entry.timed_out += counts.timed_out;
  }
}

fn display_path_for_markdown(path: &Path) -> String {
  // For committed artifacts (baseline summaries, CI step summaries), avoid embedding absolute,
  // machine-specific paths. Prefer displaying a path relative to the current working directory when
  // possible.
  let mut path = path.to_path_buf();
  if path.is_absolute() {
    if let Ok(cwd) = std::env::current_dir() {
      if let Ok(rel) = path.strip_prefix(&cwd) {
        path = rel.to_path_buf();
      }
    }
  }
  path.display().to_string().replace('\\', "/")
}

pub fn normalize_markdown(mut markdown: String) -> String {
  // Remove any accidental trailing whitespace while keeping predictable newlines.
  let lines: Vec<String> = markdown
    .lines()
    .map(|line| line.trim_end().to_string())
    .collect();
  markdown = lines.join("\n");
  if !markdown.ends_with('\n') {
    markdown.push('\n');
  }
  markdown
}

pub fn write_markdown(path: &Path, markdown: &str) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  }

  let mut normalized = markdown.to_string();
  normalized = normalize_markdown(mem::take(&mut normalized));
  fs::write(path, normalized.as_bytes()).with_context(|| format!("write {}", path.display()))?;
  Ok(())
}
