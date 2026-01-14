use anyhow::{bail, Context, Result};
use clap::{Args, Parser};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_PROGRESS_DIR: &str = "progress/pages";
const DEFAULT_OUT_DIR: &str = "target/refresh_progress_accuracy";
// Pageset progress accuracy is computed against Chrome screenshots rendered at 1200×800 (see
// `progress/pages/README.md`). Keep `refresh-progress-accuracy` aligned with that so syncing metrics
// does not accidentally switch the viewport and invalidate historical comparisons.
const DEFAULT_VIEWPORT: &str = "1200x800";
// Some pageset fixtures (notably large news sites like dailymail.co.uk) can take tens of seconds to
// render; use the same generous timeout as `xtask page-loop` so accuracy refreshes do not spuriously
// time out.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Args, Debug)]
pub struct RefreshProgressAccuracyArgs {
  /// Directory containing committed `progress/pages/*.json`.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_PROGRESS_DIR)]
  pub progress_dir: PathBuf,

  /// Root directory to write fixture-vs-Chrome diff report artifacts into.
  ///
  /// The diff report JSON used for syncing is written to `<out-dir>/report.json`.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_OUT_DIR)]
  pub out_dir: PathBuf,

  /// Skip building renderer binaries and reuse existing binaries under the selected Cargo profile.
  ///
  /// This forwards `--no-build` to `xtask fixture-chrome-diff`.
  #[arg(long)]
  pub no_build: bool,

  /// Only process listed fixture names (comma-separated stems).
  ///
  /// This forwards `--fixtures` to `xtask fixture-chrome-diff`.
  #[arg(
    long,
    value_delimiter = ',',
    value_name = "STEM,...",
    conflicts_with = "from_progress"
  )]
  pub fixtures: Option<Vec<String>>,

  /// Select fixtures based on pageset progress files in this directory (typically `progress/pages`).
  ///
  /// This forwards `--from-progress` to `xtask fixture-chrome-diff`.
  #[arg(long, value_name = "DIR", conflicts_with = "fixtures")]
  pub from_progress: Option<PathBuf>,

  /// When selecting from progress, include pages whose `status != ok`.
  ///
  /// If `--from-progress` is omitted, this defaults to `--progress-dir`.
  #[arg(long, conflicts_with = "fixtures")]
  pub only_failures: bool,

  /// When selecting from progress, include the top N worst ok pages with accuracy metrics.
  ///
  /// If `--from-progress` is omitted, this defaults to `--progress-dir`.
  #[arg(long, value_name = "N", conflicts_with = "fixtures")]
  pub top_worst_accuracy: Option<usize>,

  /// Minimum `accuracy.diff_percent` required when selecting via `--top-worst-accuracy`.
  #[arg(long, value_name = "PERCENT", requires = "top_worst_accuracy")]
  pub min_diff_percent: Option<f64>,

  /// Only process a deterministic shard of the fixtures (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,

  /// Per-channel tolerance forwarded to `diff_renders`.
  #[arg(long, default_value_t = 0)]
  pub tolerance: u8,

  /// Maximum percent of pixels allowed to differ (0-100) forwarded to `diff_renders`.
  #[arg(long, default_value_t = 0.0, value_name = "PERCENT")]
  pub max_diff_percent: f64,

  /// Per-fixture hard timeout in seconds forwarded to `xtask fixture-chrome-diff --timeout`.
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  pub timeout: u64,

  /// Continue even if some fixtures fail to render during the FastRender step.
  ///
  /// This forwards `--keep-going` to `xtask fixture-chrome-diff` so shard refreshes can be run
  /// unattended and still produce a best-effort `report.json`.
  #[arg(long)]
  pub keep_going: bool,

  /// Ignore alpha differences forwarded to `diff_renders --ignore-alpha`.
  #[arg(long)]
  pub ignore_alpha: bool,

  /// Maximum allowed perceptual distance (0.0 = identical) forwarded to `diff_renders`.
  #[arg(long)]
  pub max_perceptual_distance: Option<f64>,

  /// Print what would run without executing Chrome/FastRender or writing files.
  #[arg(long)]
  pub dry_run: bool,

  /// Print the top N worst accuracy entries from `--progress-dir` after syncing (0 disables).
  #[arg(long, value_name = "N", default_value_t = 10)]
  pub print_top_worst: usize,
}

pub fn run_refresh_progress_accuracy(mut args: RefreshProgressAccuracyArgs) -> Result<()> {
  if !(0.0..=100.0).contains(&args.max_diff_percent) || !args.max_diff_percent.is_finite() {
    bail!("--max-diff-percent must be a finite number between 0 and 100");
  }
  if let Some(dist) = args.max_perceptual_distance {
    if !(0.0..=1.0).contains(&dist) || !dist.is_finite() {
      bail!("--max-perceptual-distance must be a finite number between 0 and 1");
    }
  }
  if let Some(n) = args.top_worst_accuracy {
    if n == 0 {
      bail!("--top-worst-accuracy must be > 0");
    }
  }
  if let Some(min) = args.min_diff_percent {
    if !(0.0..=100.0).contains(&min) || !min.is_finite() {
      bail!("--min-diff-percent must be a finite number between 0 and 100");
    }
  }

  // Keep the common `--top-worst-accuracy` usage short by defaulting `--from-progress` to
  // `--progress-dir` when the user is clearly asking for progress-driven selection.
  if args.fixtures.is_none()
    && args.from_progress.is_none()
    && (args.only_failures || args.top_worst_accuracy.is_some())
  {
    args.from_progress = Some(args.progress_dir.clone());
  }

  let repo_root = crate::repo_root();
  let progress_dir = resolve_repo_path(&repo_root, &args.progress_dir);
  let out_dir = resolve_repo_path(&repo_root, &args.out_dir);
  let report_path = out_dir.join("report.json");

  if args.dry_run {
    print_plan(&args, &out_dir, &report_path, &progress_dir);
    return Ok(());
  }

  if !progress_dir.is_dir() {
    bail!(
      "progress directory does not exist: {}",
      progress_dir.display()
    );
  }

  let before = snapshot_progress_dir(&progress_dir)?;

  let fixture_args = build_fixture_chrome_diff_args(&args)?;
  crate::fixture_chrome_diff::run_fixture_chrome_diff(fixture_args)
    .context("fixture-chrome-diff failed")?;

  let sync_args = crate::sync_progress_accuracy::SyncProgressAccuracyArgs {
    report: report_path.clone(),
    progress_dir: args.progress_dir.clone(),
    dry_run: false,
    fail_on_missing_progress: false,
  };
  crate::sync_progress_accuracy::run_sync_progress_accuracy(sync_args)
    .context("sync-progress-accuracy failed")?;

  let after = snapshot_progress_dir(&progress_dir)?;
  let updated_progress_files = count_snapshot_changes(&before, &after);

  println!();
  println!("Refresh progress accuracy summary:");
  println!("  progress files updated: {updated_progress_files}");

  if args.print_top_worst > 0 {
    println!();
    print_top_worst_accuracy(&after, args.print_top_worst)?;
  }

  Ok(())
}

fn print_plan(
  args: &RefreshProgressAccuracyArgs,
  out_dir: &Path,
  report_path: &Path,
  progress_dir: &Path,
) {
  println!("refresh-progress-accuracy plan:");
  println!("  out_dir: {}", out_dir.display());
  println!("  report: {}", report_path.display());
  println!("  progress_dir: {}", progress_dir.display());
  if let Some((index, total)) = args.shard {
    println!("  shard: {index}/{total}");
  }
  if let Some(fixtures) = &args.fixtures {
    println!("  fixtures: {}", fixtures.join(","));
  } else if let Some(from_progress) = &args.from_progress {
    println!("  from_progress: {}", from_progress.display());
    if args.only_failures {
      println!("  selection: only_failures");
    }
    if let Some(n) = args.top_worst_accuracy {
      if let Some(min) = args.min_diff_percent {
        println!("  selection: top_worst_accuracy={n} (min_diff_percent={min})");
      } else {
        println!("  selection: top_worst_accuracy={n}");
      }
    }
  } else {
    println!("  selection: default fixture set (pages_regression)");
  }
  println!("  viewport: {DEFAULT_VIEWPORT} (pageset progress baseline)");
  println!("  timeout: {}s", args.timeout);
  println!("  no_build: {}", args.no_build);
  println!(
    "  diff: tolerance={} max_diff_percent={} ignore_alpha={} max_perceptual_distance={}",
    args.tolerance,
    args.max_diff_percent,
    args.ignore_alpha,
    args
      .max_perceptual_distance
      .map(|v| v.to_string())
      .unwrap_or_else(|| "<none>".to_string())
  );
  println!("  print_top_worst: {}", args.print_top_worst);
  println!();
  println!("Steps:");
  println!(
    "  1) fixture-chrome-diff (writes {})",
    report_path.display()
  );
  if let Ok(argv) = build_fixture_chrome_diff_argv(args) {
    // Print the exact `fixture-chrome-diff` invocation the wrapper will run (useful for debugging
    // sharding/selection).
    let rendered = argv
      .iter()
      .map(|v| v.to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");
    println!("     $ {rendered}");
  }
  println!(
    "  2) sync-progress-accuracy --report {} --progress-dir {}",
    report_path.display(),
    progress_dir.display()
  );
  if args.print_top_worst > 0 {
    println!(
      "  3) print top {} worst accuracy entries",
      args.print_top_worst
    );
  }
}

fn build_fixture_chrome_diff_args(
  args: &RefreshProgressAccuracyArgs,
) -> Result<crate::fixture_chrome_diff::FixtureChromeDiffArgs> {
  let argv = build_fixture_chrome_diff_argv(args)?;

  let cli = crate::Cli::try_parse_from(argv).map_err(anyhow::Error::new)?;
  match cli.command {
    crate::Commands::FixtureChromeDiff(args) => Ok(args),
    _ => bail!("internal error: failed to parse fixture-chrome-diff args"),
  }
}

fn build_fixture_chrome_diff_argv(args: &RefreshProgressAccuracyArgs) -> Result<Vec<OsString>> {
  // Build a `fixture-chrome-diff` argument list by reusing the canonical clap definitions and
  // defaults. This avoids duplicating its (large) set of defaults and keeps the wrapper aligned
  // with future changes.
  let mut argv: Vec<OsString> = vec!["xtask".into(), "fixture-chrome-diff".into()];
  if args.no_build {
    argv.push("--no-build".into());
  }
  argv.push("--out-dir".into());
  argv.push(args.out_dir.as_os_str().to_os_string());

  // `refresh-progress-accuracy` is intended to be a lightweight "sync committed metrics" tool.
  // Use the debug profile for fixture rendering/diffing to avoid compiling huge release binaries
  // when refreshing a small subset of pages.
  argv.push("--debug".into());

  argv.push("--viewport".into());
  argv.push(DEFAULT_VIEWPORT.into());
  if args.keep_going {
    argv.push("--keep-going".into());
  }

  if let Some(fixtures) = &args.fixtures {
    argv.push("--fixtures".into());
    argv.push(fixtures.join(",").into());
  }
  if let Some(from_progress) = &args.from_progress {
    argv.push("--from-progress".into());
    argv.push(from_progress.as_os_str().to_os_string());
  }
  if args.only_failures {
    argv.push("--only-failures".into());
  }
  if let Some(n) = args.top_worst_accuracy {
    argv.push("--top-worst-accuracy".into());
    argv.push(n.to_string().into());
  }
  if let Some(min) = args.min_diff_percent {
    argv.push("--min-diff-percent".into());
    argv.push(min.to_string().into());
  }

  if let Some((index, total)) = args.shard {
    argv.push("--shard".into());
    argv.push(format!("{index}/{total}").into());
  }

  argv.push("--tolerance".into());
  argv.push(args.tolerance.to_string().into());
  argv.push("--max-diff-percent".into());
  argv.push(args.max_diff_percent.to_string().into());
  argv.push("--timeout".into());
  argv.push(args.timeout.to_string().into());
  if let Some(dist) = args.max_perceptual_distance {
    argv.push("--max-perceptual-distance".into());
    argv.push(dist.to_string().into());
  }
  if args.ignore_alpha {
    argv.push("--ignore-alpha".into());
  }

  Ok(argv)
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn snapshot_progress_dir(progress_dir: &Path) -> Result<BTreeMap<String, String>> {
  let mut snapshot = BTreeMap::new();
  for entry in fs::read_dir(progress_dir)
    .with_context(|| format!("read progress directory {}", progress_dir.display()))?
  {
    let entry = entry.context("read progress dir entry")?;
    let file_type = entry.file_type().context("read progress dir entry type")?;
    if !file_type.is_file() {
      continue;
    }
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("json") {
      continue;
    }
    let stem = path
      .file_stem()
      .and_then(|s| s.to_str())
      .context("progress JSON filename should be UTF-8")?
      .to_string();
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    snapshot.insert(stem, raw);
  }
  Ok(snapshot)
}

fn count_snapshot_changes(
  before: &BTreeMap<String, String>,
  after: &BTreeMap<String, String>,
) -> usize {
  let mut updated = 0usize;
  for (k, v) in before {
    if after.get(k) != Some(v) {
      updated += 1;
    }
  }
  for k in after.keys() {
    if !before.contains_key(k) {
      updated += 1;
    }
  }
  updated
}

#[derive(Debug)]
struct AccuracyEntry {
  stem: String,
  diff_percent: f64,
  perceptual: f64,
  first_mismatch: Option<AccuracyFirstMismatch>,
}

#[derive(Debug)]
struct AccuracyFirstMismatch {
  x: u32,
  y: u32,
  baseline_rgba: Option<[u8; 4]>,
  rendered_rgba: Option<[u8; 4]>,
}

fn parse_rgba_array(value: &Value) -> Option<[u8; 4]> {
  let arr = value.as_array()?;
  if arr.len() != 4 {
    return None;
  }
  let mut out = [0u8; 4];
  for (idx, item) in arr.iter().enumerate() {
    let channel = item.as_u64()?;
    if channel > u8::MAX as u64 {
      return None;
    }
    out[idx] = channel as u8;
  }
  Some(out)
}

fn parse_first_mismatch(value: &Value) -> Option<AccuracyFirstMismatch> {
  let obj = value.as_object()?;
  let x = obj.get("x")?.as_u64()?;
  let y = obj.get("y")?.as_u64()?;
  let x = u32::try_from(x).ok()?;
  let y = u32::try_from(y).ok()?;
  let baseline_rgba = obj.get("baseline_rgba").and_then(parse_rgba_array);
  let rendered_rgba = obj.get("rendered_rgba").and_then(parse_rgba_array);
  Some(AccuracyFirstMismatch {
    x,
    y,
    baseline_rgba,
    rendered_rgba,
  })
}

fn format_top_worst_accuracy(snapshot: &BTreeMap<String, String>, n: usize) -> Result<String> {
  let mut entries = Vec::<AccuracyEntry>::new();
  for (stem, raw) in snapshot {
    let json: Value =
      serde_json::from_str(raw).with_context(|| format!("parse progress JSON for {stem}"))?;
    let status = json
      .get("status")
      .and_then(|v| v.as_str())
      .unwrap_or_default();
    if status != "ok" {
      continue;
    }
    let accuracy = json.get("accuracy").and_then(|v| v.as_object());
    let Some(accuracy) = accuracy else {
      continue;
    };
    let Some(diff_percent) = accuracy.get("diff_percent").and_then(|v| v.as_f64()) else {
      continue;
    };
    let perceptual = accuracy
      .get("perceptual")
      .and_then(|v| v.as_f64())
      .unwrap_or(0.0);
    let first_mismatch = accuracy
      .get("first_mismatch")
      .and_then(parse_first_mismatch);
    entries.push(AccuracyEntry {
      stem: stem.clone(),
      diff_percent,
      perceptual,
      first_mismatch,
    });
  }

  entries.sort_by(|a, b| {
    b.diff_percent
      .partial_cmp(&a.diff_percent)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then_with(|| {
        b.perceptual
          .partial_cmp(&a.perceptual)
          .unwrap_or(std::cmp::Ordering::Equal)
      })
      .then_with(|| a.stem.cmp(&b.stem))
  });

  let mut out = String::new();
  out.push_str(&format!("Top {n} worst accuracy entries (status=ok):\n"));
  for entry in entries.into_iter().take(n) {
    let mut line = format!(
      "  - {}: diff_percent={:.4}%, perceptual={:.4}",
      entry.stem, entry.diff_percent, entry.perceptual
    );
    if let Some(mismatch) = entry.first_mismatch.as_ref() {
      line.push_str(&format!(" first_mismatch=({}, {})", mismatch.x, mismatch.y));
      if let (Some(baseline), Some(rendered)) = (mismatch.baseline_rgba, mismatch.rendered_rgba) {
        line.push_str(&format!(
          " baseline_rgba={baseline:?} rendered_rgba={rendered:?}"
        ));
      }
    }
    out.push_str(&line);
    out.push('\n');
  }
  Ok(out)
}

fn print_top_worst_accuracy(snapshot: &BTreeMap<String, String>, n: usize) -> Result<()> {
  print!("{}", format_top_worst_accuracy(snapshot, n)?);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn format_top_worst_accuracy_includes_first_mismatch_when_available() {
    let mut snapshot = BTreeMap::<String, String>::new();
    snapshot.insert(
      "a".to_string(),
      serde_json::json!({
        "status": "ok",
        "accuracy": {
          "diff_percent": 50.0,
          "perceptual": 0.5,
          "first_mismatch": {
            "x": 1,
            "y": 2,
            "baseline_rgba": [1, 2, 3, 4],
            "rendered_rgba": [250, 251, 252, 253]
          }
        }
      })
      .to_string(),
    );
    snapshot.insert(
      "b".to_string(),
      serde_json::json!({
        "status": "ok",
        "accuracy": {
          "diff_percent": 60.0,
          "perceptual": 0.2
        }
      })
      .to_string(),
    );
    snapshot.insert(
      "c".to_string(),
      serde_json::json!({
        "status": "error",
        "accuracy": {
          "diff_percent": 99.0,
          "perceptual": 1.0
        }
      })
      .to_string(),
    );

    let formatted = format_top_worst_accuracy(&snapshot, 2).expect("format");
    let expected = concat!(
      "Top 2 worst accuracy entries (status=ok):\n",
      "  - b: diff_percent=60.0000%, perceptual=0.2000\n",
      "  - a: diff_percent=50.0000%, perceptual=0.5000 first_mismatch=(1, 2) baseline_rgba=[1, 2, 3, 4] rendered_rgba=[250, 251, 252, 253]\n",
    );
    assert_eq!(formatted, expected);
  }

  #[test]
  fn build_fixture_chrome_diff_args_does_not_duplicate_timeout_flag() {
    let args = RefreshProgressAccuracyArgs {
      progress_dir: PathBuf::from("progress/pages"),
      out_dir: PathBuf::from("target/refresh_progress_accuracy_test"),
      fixtures: Some(vec!["gentoo.org".to_string()]),
      from_progress: None,
      only_failures: false,
      top_worst_accuracy: None,
      min_diff_percent: None,
      shard: None,
      tolerance: 0,
      max_diff_percent: 0.0,
      timeout: 123,
      keep_going: false,
      ignore_alpha: false,
      max_perceptual_distance: None,
      dry_run: false,
      print_top_worst: 0,
    };

    let diff_args = build_fixture_chrome_diff_args(&args).expect("fixture-chrome-diff args");
    assert_eq!(diff_args.viewport, (1200, 800));
    assert_eq!(diff_args.timeout, 123);
  }
}
