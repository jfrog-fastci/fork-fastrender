use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use conformance_harness::{write_json_report, FailOn, Shard};
use js_wpt_dom_runner::{run_suite, BackendSelection, SuiteConfig};
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
pub enum WptDomSuite {
  /// Run the curated DOM + event-loop subset (defaults to `event*/**`).
  Curated,
  /// Run the harness bring-up smoke subset (`smoke/**`).
  Smoke,
  /// Run the full corpus (curated + smoke).
  All,
}

impl WptDomSuite {
  fn default_filter(self) -> Option<&'static str> {
    match self {
      // The smoke subset contains intentional failures for harness validation; keep it out of the
      // default "curated" run so `xtask js wpt-dom` stays green.
      Self::Curated => Some("event*/**"),
      Self::Smoke => Some("smoke/**"),
      Self::All => None,
    }
  }
}

#[derive(Args, Debug)]
pub struct WptDomArgs {
  /// Select which preset suite to run.
  #[arg(long, value_enum, default_value_t = WptDomSuite::Curated)]
  pub suite: WptDomSuite,

  /// Root directory containing the offline WPT DOM corpus (`tests/`, `resources/`, expectations).
  #[arg(long, value_name = "DIR", default_value = DEFAULT_WPT_ROOT)]
  pub wpt_root: PathBuf,

  /// Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,

  /// Run only a deterministic shard of the corpus (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,

  /// Filter tests by id using a glob or regex (glob is attempted first).
  #[arg(long, value_name = "GLOB|REGEX")]
  pub filter: Option<String>,

  /// Per-test timeout (milliseconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS, value_name = "MS")]
  pub timeout_ms: u64,

  /// Per-test timeout (seconds).
  ///
  /// Overrides `--timeout-ms` (which defaults to 5000ms).
  #[arg(long, value_name = "SECS")]
  pub timeout_secs: Option<u64>,

  /// Timeout used when a test specifies `timeout=long` (milliseconds).
  #[arg(long, default_value_t = DEFAULT_LONG_TIMEOUT_MS, value_name = "MS")]
  pub long_timeout_ms: u64,

  /// Timeout used when a test specifies `timeout=long` (seconds).
  ///
  /// Overrides `--long-timeout-ms` (which defaults to 30000ms).
  #[arg(long, value_name = "SECS")]
  pub long_timeout_secs: Option<u64>,

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
  let timeout_ms = if let Some(secs) = args.timeout_secs {
    if secs == 0 {
      bail!("--timeout-secs must be > 0");
    }
    secs
      .checked_mul(1000)
      .context("--timeout-secs overflow (value too large)")?
  } else {
    if args.timeout_ms == 0 {
      bail!("--timeout-ms must be > 0");
    }
    args.timeout_ms
  };

  let long_timeout_ms = if let Some(secs) = args.long_timeout_secs {
    if secs == 0 {
      bail!("--long-timeout-secs must be > 0");
    }
    secs
      .checked_mul(1000)
      .context("--long-timeout-secs overflow (value too large)")?
  } else {
    if args.long_timeout_ms == 0 {
      bail!("--long-timeout-ms must be > 0");
    }
    args.long_timeout_ms
  };

  let repo_root = crate::repo_root();

  let wpt_root = resolve_repo_path(&repo_root, &args.wpt_root);
  ensure_wpt_root(&wpt_root)?;

  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  if !manifest_path.is_file() {
    bail!(
      "expectations manifest {} does not exist",
      manifest_path.display()
    );
  }

  let report_path = resolve_repo_path(&repo_root, &args.report);

  let shard = args.shard.map(|(index, total)| Shard { index, total });
  let filter = args
    .filter
    .clone()
    .or_else(|| args.suite.default_filter().map(ToString::to_string));

  println!("Running WPT DOM suite ({:?})...", args.suite);

  let report = run_suite(&SuiteConfig {
    wpt_root: wpt_root.clone(),
    manifest_path: manifest_path.clone(),
    shard,
    filter,
    timeout: Duration::from_millis(timeout_ms),
    long_timeout: Duration::from_millis(long_timeout_ms),
    fail_on: args.fail_on,
    backend: args.backend.to_selection(),
  })
  .context("run WPT DOM suite")?;

  write_json_report(&report_path, &report)
    .with_context(|| format!("write report to {}", report_path.display()))?;

  println!("WPT DOM suite summary:");
  println!("  total: {}", report.summary.total);
  println!("  passed: {}", report.summary.passed);
  println!("  failed: {}", report.summary.failed);
  println!("  timed_out: {}", report.summary.timed_out);
  println!("  errored: {}", report.summary.errored);
  println!("  skipped: {}", report.summary.skipped);
  if let Some(m) = &report.summary.mismatches {
    println!("  mismatches:");
    println!("    expected: {}", m.expected);
    println!("    unexpected: {}", m.unexpected);
    println!("    flaky: {}", m.flaky);
  }
  println!("JSON report: {}", report_path.display());

  if report.summary.should_fail(args.fail_on) {
    bail!(
      "WPT DOM suite did not satisfy fail-on={:?}; see {}",
      args.fail_on,
      report_path.display()
    );
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

fn ensure_wpt_root(wpt_root: &Path) -> Result<()> {
  let tests_dir = wpt_root.join("tests");
  let resources_dir = wpt_root.join("resources");
  if tests_dir.is_dir() && resources_dir.is_dir() {
    return Ok(());
  }

  bail!(
    "WPT DOM corpus root {} is missing required directories.\n\
     Expected:\n\
       - {}/\n\
       - {}/\n\
     If you are missing this corpus, check out the FastRender repo with the bundled `tests/wpt_dom/` directory.",
    wpt_root.display(),
    tests_dir.display(),
    resources_dir.display()
  );
}
