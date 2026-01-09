use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use conformance_harness::{FailOn as HarnessFailOn, Shard};
use js_wpt_dom_runner::{run_suite, BackendSelection, SuiteConfig};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_WPT_ROOT: &str = "tests/wpt_dom";
const DEFAULT_MANIFEST_PATH: &str = "tests/wpt_dom/expectations.toml";
const DEFAULT_REPORT_PATH: &str = "target/js/wpt_dom.json";

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_LONG_TIMEOUT_MS: u64 = 30_000;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum WptDomBackend {
  /// Prefer the best backend available in the current build (vm-js when enabled).
  Auto,
  /// QuickJS backend (deterministic host timers + JS shims).
  QuickJs,
  /// vm-js backend (FastRender in-tree JS runtime; currently feature-gated).
  VmJs,
}

impl WptDomBackend {
  fn to_selection(self) -> BackendSelection {
    match self {
      WptDomBackend::Auto => BackendSelection::Auto,
      WptDomBackend::QuickJs => BackendSelection::QuickJs,
      WptDomBackend::VmJs => BackendSelection::VmJs,
    }
  }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum FailOn {
  /// Fail on any mismatch (including expected/xfail/flaky).
  All,
  /// Fail only on unexpected mismatches (default).
  New,
  /// Never fail based on mismatches (always exit 0).
  None,
}

impl FailOn {
  fn to_harness(self) -> HarnessFailOn {
    match self {
      FailOn::All => HarnessFailOn::All,
      FailOn::New => HarnessFailOn::New,
      FailOn::None => HarnessFailOn::None,
    }
  }
}

#[derive(Args, Debug)]
pub struct WptDomArgs {
  /// Path to the offline WPT DOM corpus root (defaults to `tests/wpt_dom`).
  #[arg(long, value_name = "DIR", default_value = DEFAULT_WPT_ROOT)]
  pub wpt_root: PathBuf,

  /// Override the expectations manifest (skip/xfail/flaky).
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,

  /// Run only a deterministic shard of the corpus (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,

  /// Filter test ids using a glob (preferred) or regex.
  #[arg(long, value_name = "PATTERN")]
  pub filter: Option<String>,

  /// Per-test timeout (milliseconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS, value_name = "MS")]
  pub timeout_ms: u64,

  /// Per-test timeout for `timeout=long` tests (milliseconds).
  #[arg(long, default_value_t = DEFAULT_LONG_TIMEOUT_MS, value_name = "MS")]
  pub long_timeout_ms: u64,

  /// Control which mismatches cause a non-zero exit code.
  #[arg(long, default_value_t = FailOn::New, value_enum)]
  pub fail_on: FailOn,

  /// JSON report output path.
  #[arg(
    long,
    visible_alias = "report-path",
    value_name = "PATH",
    default_value = DEFAULT_REPORT_PATH
  )]
  pub report: PathBuf,

  /// Select which JS backend to execute the corpus with.
  #[arg(long, value_enum, default_value_t = WptDomBackend::Auto)]
  pub backend: WptDomBackend,
}

pub fn run_wpt_dom(args: WptDomArgs) -> Result<()> {
  if args.timeout_ms == 0 {
    bail!("--timeout-ms must be > 0");
  }
  if args.long_timeout_ms == 0 {
    bail!("--long-timeout-ms must be > 0");
  }

  let repo_root = crate::repo_root();
  let wpt_root = resolve_repo_path(&repo_root, &args.wpt_root);
  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  let report_path = resolve_repo_path(&repo_root, &args.report);

  if !wpt_root.is_dir() {
    bail!("wpt corpus root {} does not exist", wpt_root.display());
  }
  if !manifest_path.is_file() {
    bail!("expectations manifest {} does not exist", manifest_path.display());
  }
  if let Some(parent) = report_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }

  let shard = args
    .shard
    .map(|(index, total)| Shard { index, total });

  let fail_on = args.fail_on.to_harness();
  let backend = args.backend.to_selection();

  let report = run_suite(&SuiteConfig {
    wpt_root: wpt_root.clone(),
    manifest_path: manifest_path.clone(),
    shard,
    filter: args.filter.clone(),
    timeout: Duration::from_millis(args.timeout_ms),
    long_timeout: Duration::from_millis(args.long_timeout_ms),
    fail_on,
    backend,
  })
  .context("run WPT DOM suite")?;

  let json = serde_json::to_string_pretty(&report).context("serialize report")?;
  fs::write(&report_path, json).with_context(|| format!("write {}", report_path.display()))?;

  println!(
    "WPT DOM suite done: total={}, passed={}, failed={}, timed_out={}, errored={}, skipped={}",
    report.summary.total,
    report.summary.passed,
    report.summary.failed,
    report.summary.timed_out,
    report.summary.errored,
    report.summary.skipped
  );
  if let Some(m) = &report.summary.mismatches {
    println!(
      "Mismatches: expected={}, unexpected={}, flaky={}, total={}",
      m.expected,
      m.unexpected,
      m.flaky,
      m.total()
    );
  }
  println!("JSON report: {}", report_path.display());

  if js_wpt_dom_runner::should_fail(&report.summary, fail_on) {
    bail!("wpt-dom suite has mismatches (see {})", report_path.display());
  }

  Ok(())
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

