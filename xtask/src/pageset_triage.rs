use anyhow::{bail, Context, Result};
use clap::Args;
use fastrender::cli_utils::report::entry_anchor_id;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
 
const DEFAULT_PROGRESS_DIR: &str = "progress/pages";
const DEFAULT_OUT_PATH: &str = "target/pageset_triage/report.md";
const DEFAULT_FIXTURE_INDEX_ROOT: &str = "tests/pages/fixtures";
 
#[derive(Args, Debug)]
pub struct PagesetTriageArgs {
  /// Directory containing committed `progress/pages/*.json`.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_PROGRESS_DIR)]
  pub progress_dir: PathBuf,
 
  /// Optional `diff_renders` JSON report (typically `target/fixture_chrome_diff/report.json`).
  #[arg(long, value_name = "PATH")]
  pub report: Option<PathBuf>,
 
  /// Only include these page stems (comma-separated).
  #[arg(long = "only", value_delimiter = ',', value_name = "STEM,...")]
  pub only_pages: Option<Vec<String>>,
 
  /// Include the top N worst-accuracy pages (by `accuracy.diff_percent` / diff report `metrics.diff_percentage`).
  #[arg(long, value_name = "N")]
  pub top_worst_accuracy: Option<usize>,
 
  /// Include the top N slowest pages (by `total_ms`).
  #[arg(long, value_name = "N")]
  pub top_slowest: Option<usize>,
 
  /// Where to write the Markdown report.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_OUT_PATH)]
  pub out: PathBuf,
}
 
#[derive(Debug, Clone)]
struct PageTriageRow {
  stem: String,
  url: Option<String>,
  status: Option<String>,
  hotspot: Option<String>,
  total_ms: Option<f64>,
  notes: Option<String>,
  auto_notes: Option<String>,
  accuracy: Option<ProgressAccuracy>,
}
 
impl PageTriageRow {
  fn progress_accuracy_summary(&self) -> Option<(f64, f64)> {
    let accuracy = self.accuracy.as_ref()?;
    let diff_percent = accuracy.diff_percent.filter(|v| v.is_finite())?;
    let perceptual = accuracy.perceptual.filter(|v| v.is_finite())?;
    Some((diff_percent, perceptual))
  }
}
 
#[derive(Debug, Clone, Deserialize)]
struct ProgressPage {
  #[serde(default)]
  url: Option<String>,
  #[serde(default)]
  status: Option<String>,
  #[serde(default)]
  hotspot: Option<String>,
  #[serde(default)]
  total_ms: Option<f64>,
  #[serde(default)]
  notes: Option<String>,
  #[serde(default)]
  auto_notes: Option<String>,
  #[serde(default)]
  accuracy: Option<ProgressAccuracy>,
}
 
#[derive(Debug, Clone, Deserialize)]
struct ProgressAccuracy {
  #[serde(default)]
  diff_percent: Option<f64>,
  #[serde(default)]
  perceptual: Option<f64>,
}
 
#[derive(Debug, Deserialize)]
struct DiffReport {
  results: Vec<DiffReportEntry>,
}
 
#[derive(Debug, Deserialize, Clone)]
struct DiffReportEntry {
  name: String,
  status: EntryStatus,
  #[serde(default)]
  before: Option<String>,
  #[serde(default)]
  after: Option<String>,
  #[serde(default)]
  diff: Option<String>,
  #[serde(default)]
  metrics: Option<DiffMetrics>,
  #[serde(default)]
  error: Option<String>,
}
 
#[derive(Debug, Deserialize, Clone)]
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
  fn as_str(&self) -> &'static str {
    match self {
      Self::Match => "match",
      Self::WithinThreshold => "within_threshold",
      Self::Diff => "diff",
      Self::MissingBefore => "missing_before",
      Self::MissingAfter => "missing_after",
      Self::Error => "error",
    }
  }
}

#[derive(Debug, Deserialize, Clone)]
struct DiffMetrics {
  diff_percentage: f64,
  perceptual_distance: f64,
}
 
pub fn run_pageset_triage(mut args: PagesetTriageArgs) -> Result<()> {
  let repo_root = crate::repo_root();
 
  if !args.progress_dir.is_absolute() {
    args.progress_dir = repo_root.join(&args.progress_dir);
  }
  if let Some(report) = args.report.as_mut() {
    if !report.is_absolute() {
      *report = repo_root.join(&*report);
    }
  }
  if !args.out.is_absolute() {
    args.out = repo_root.join(&args.out);
  }
 
  let diff_entries = match args.report {
    Some(report) => Some(read_diff_report(&report)?),
    None => None,
  };
 
  let all_pages = read_progress_pages(&args.progress_dir)?;
  let filtered_pages = filter_pages(all_pages, args.only_pages.as_deref())?;
  let selected_pages = select_pages(
    filtered_pages,
    args.top_worst_accuracy,
    args.top_slowest,
    diff_entries.as_ref(),
  );
 
  let markdown = render_markdown(&selected_pages, diff_entries.as_ref());
 
  if let Some(parent) = args.out.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)
        .with_context(|| format!("create output dir {}", parent.display()))?;
    }
  }
  fs::write(&args.out, markdown.as_bytes())
    .with_context(|| format!("write {}", args.out.display()))?;
 
  println!("Wrote triage report to {}", args.out.display());
  Ok(())
}
 
fn read_progress_pages(progress_dir: &Path) -> Result<Vec<PageTriageRow>> {
  let mut pages = Vec::new();
 
  for entry in fs::read_dir(progress_dir)
    .with_context(|| format!("read progress dir {}", progress_dir.display()))?
  {
    let entry = entry
      .with_context(|| format!("read directory entry in {}", progress_dir.display()))?;
    let path = entry.path();
 
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
      continue;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
      continue;
    };
 
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let page: ProgressPage =
      serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
 
    pages.push(PageTriageRow {
      stem: stem.to_string(),
      url: page.url,
      status: page.status,
      hotspot: page.hotspot,
      total_ms: page.total_ms,
      notes: page.notes,
      auto_notes: page.auto_notes,
      accuracy: page.accuracy,
    });
  }
 
  pages.sort_by(|a, b| a.stem.cmp(&b.stem));
  Ok(pages)
}
 
fn filter_pages(pages: Vec<PageTriageRow>, only: Option<&[String]>) -> Result<Vec<PageTriageRow>> {
  let Some(only) = only else {
    return Ok(pages);
  };
 
  let only_set: BTreeSet<&str> = only.iter().map(|s| s.as_str()).collect();
  let mut missing: Vec<String> = only
    .iter()
    .filter(|stem| !pages.iter().any(|p| p.stem == **stem))
    .cloned()
    .collect();
  missing.sort();
  if !missing.is_empty() {
    bail!("requested page stems not found under progress dir: {}", missing.join(", "));
  }
 
  let mut filtered: Vec<PageTriageRow> = pages
    .into_iter()
    .filter(|p| only_set.contains(p.stem.as_str()))
    .collect();
  filtered.sort_by(|a, b| a.stem.cmp(&b.stem));
  Ok(filtered)
}
 
fn select_pages(
  pages: Vec<PageTriageRow>,
  top_worst_accuracy: Option<usize>,
  top_slowest: Option<usize>,
  diff_entries: Option<&BTreeMap<String, DiffReportEntry>>,
) -> Vec<PageTriageRow> {
  if top_worst_accuracy.is_none() && top_slowest.is_none() {
    return pages;
  }
 
  let mut selected_stems = BTreeSet::<String>::new();
  let mut selected = Vec::<PageTriageRow>::new();
 
  if let Some(n) = top_worst_accuracy {
    let mut ranked: Vec<(f64, f64, &PageTriageRow)> = pages
      .iter()
      .filter_map(|p| {
        if let Some((diff_percent, perceptual)) = p.progress_accuracy_summary() {
          return Some((diff_percent, perceptual, p));
        }
 
        let entry = diff_entries?.get(&p.stem)?;
        let metrics = entry.metrics.as_ref()?;
        let diff_percent = metrics.diff_percentage;
        let perceptual = metrics.perceptual_distance;
        if !diff_percent.is_finite() || !perceptual.is_finite() {
          return None;
        }
        Some((diff_percent, perceptual, p))
      })
      .collect();
 
    ranked.sort_by(|a, b| {
      b.0
        .total_cmp(&a.0)
        .then_with(|| b.1.total_cmp(&a.1))
        .then_with(|| a.2.stem.cmp(&b.2.stem))
    });
 
    for (_, _, page) in ranked.into_iter().take(n) {
      if selected_stems.insert(page.stem.clone()) {
        selected.push(page.clone());
      }
    }
  }
 
  if let Some(n) = top_slowest {
    let mut ranked: Vec<(f64, &PageTriageRow)> = pages
      .iter()
      .filter_map(|p| p.total_ms.filter(|v| v.is_finite()).map(|ms| (ms, p)))
      .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.stem.cmp(&b.1.stem)));
 
    for (_, page) in ranked.into_iter().take(n) {
      if selected_stems.insert(page.stem.clone()) {
        selected.push(page.clone());
      }
    }
  }
 
  selected
}
 
fn read_diff_report(report_path: &Path) -> Result<BTreeMap<String, DiffReportEntry>> {
  let raw = fs::read_to_string(report_path)
    .with_context(|| format!("read diff report {}", report_path.display()))?;
  let report: DiffReport =
    serde_json::from_str(&raw).context("parse diff_renders report JSON")?;
 
  let mut map = BTreeMap::new();
  for entry in report.results {
    map.insert(entry.name.clone(), entry);
  }
  Ok(map)
}
 
fn render_markdown(
  pages: &[PageTriageRow],
  diff_entries: Option<&BTreeMap<String, DiffReportEntry>>,
) -> String {
  let mut out = String::new();
 
  out.push_str("# Pageset triage report\n\n");
  out.push_str("This is an editable template. Fill in the **Brokenness inventory** section for each page.\n\n");
  out.push_str(&format!("Pages: {}\n\n", pages.len()));
 
  for (idx, page) in pages.iter().enumerate() {
    if idx > 0 {
      out.push('\n');
    }
    out.push_str(&render_page_section(page, diff_entries.and_then(|m| m.get(&page.stem))));
  }
 
  out
}
 
fn render_page_section(page: &PageTriageRow, diff: Option<&DiffReportEntry>) -> String {
  let mut out = String::new();
  out.push_str(&format!("## {}\n\n", page.stem));
 
  out.push_str(&format!(
    "- URL: {}\n",
    page.url.as_deref().unwrap_or("n/a")
  ));
  out.push_str(&format!(
    "- Fixture: `{}/{}/index.html`\n",
    DEFAULT_FIXTURE_INDEX_ROOT, page.stem
  ));
 
  out.push_str("- Progress: ");
  out.push_str(&format!(
    "status={} ",
    page.status.as_deref().unwrap_or("n/a")
  ));
  out.push_str(&format!(
    "hotspot={} ",
    page.hotspot.as_deref().unwrap_or("n/a")
  ));
  match page.total_ms.filter(|v| v.is_finite()) {
    Some(ms) => out.push_str(&format!("total_ms={:.2}\n", ms)),
    None => out.push_str("total_ms=n/a\n"),
  }
 
  if let Some(notes) = page.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
    out.push_str(&format!("- Notes: {}\n", notes));
  }
  if let Some(auto_notes) = page
    .auto_notes
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty())
  {
    out.push_str(&format!("- Auto notes: {}\n", auto_notes));
  }
 
  match (
    page.progress_accuracy_summary(),
    diff.and_then(|e| e.metrics.as_ref()),
  ) {
    (Some((diff_percent, perceptual)), _) => {
      out.push_str(&format!(
        "- Accuracy: diff_percent={:.4}% perceptual={:.4}\n",
        diff_percent, perceptual
      ));
    }
    (None, Some(metrics)) if metrics.diff_percentage.is_finite() && metrics.perceptual_distance.is_finite() => {
      out.push_str(&format!(
        "- Accuracy (from diff report): diff_percent={:.4}% perceptual={:.4}\n",
        metrics.diff_percentage, metrics.perceptual_distance
      ));
    }
    _ => {}
  }
 
  if let Some(entry) = diff {
    out.push_str(&format!(
      "- Diff report: status={} (`report.html#{}`)\n",
      entry.status.as_str(),
      entry_anchor_id(&entry.name)
    ));
 
    if let Some(before) = entry.before.as_deref() {
      out.push_str(&format!("  - before: `{before}`\n"));
    }
    if let Some(after) = entry.after.as_deref() {
      out.push_str(&format!("  - after: `{after}`\n"));
    }
    if let Some(diff) = entry.diff.as_deref() {
      out.push_str(&format!("  - diff: `{diff}`\n"));
    }
    if let Some(error) = entry.error.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
      out.push_str(&format!("  - error: {}\n", error));
    }
  }
 
  out.push_str("\n### Brokenness inventory\n");
  out.push_str("- Layout:\n  - [ ] ...\n");
  out.push_str("- Text:\n  - [ ] ...\n");
  out.push_str("- Paint:\n  - [ ] ...\n");
  out.push_str("- Resources:\n  - [ ] ...\n");

  out
}
