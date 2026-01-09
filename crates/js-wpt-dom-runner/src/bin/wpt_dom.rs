use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use conformance_harness::{FailOn, Shard};
use js_wpt_dom_runner::{run_suite, BackendSelection, SuiteConfig};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_WPT_ROOT: &str = "tests/wpt_dom";
const DEFAULT_MANIFEST_PATH: &str = "tests/wpt_dom/expectations.toml";
const DEFAULT_REPORT_PATH: &str = "target/js/wpt_dom.json";

const DEFAULT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_LONG_TIMEOUT_SECS: u64 = 30;

const BACKEND_ENV_VAR: &str = "FASTERENDER_WPT_DOM_BACKEND";

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum SuitePreset {
  /// Curated "real" DOM + event-loop tests (excludes harness bring-up smoke cases).
  Curated,
  /// Harness bring-up smoke tests (includes intentional failures).
  Smoke,
  /// Run the full corpus (curated + smoke).
  All,
}

impl SuitePreset {
  fn default_filter(self) -> Option<&'static str> {
    match self {
      // Curated suite selection depends on which backends are available:
      // - QuickJS-only builds do not expose DOM/EventTarget shims yet, so they run only the
      //   `event_loop/**` coverage.
      // - vm-js builds run the full curated corpus (`event_loop/**` + `events/**`).
      SuitePreset::Curated => {
        if cfg!(feature = "vmjs") {
          Some("event*/**")
        } else {
          Some("event_loop/**")
        }
      }
      SuitePreset::Smoke => Some("smoke/**"),
      SuitePreset::All => None,
    }
  }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum Backend {
  /// Let the runner pick (currently only vm-js).
  Auto,
  /// Force the vm-js backend.
  Vmjs,
}

impl Backend {
  fn as_env_value(self) -> &'static str {
    match self {
      Backend::Auto => "auto",
      Backend::Vmjs => "vmjs",
    }
  }
}

#[derive(Parser, Debug)]
#[command(
  name = "wpt_dom",
  about = "Run FastRender's offline WPT DOM (testharness.js) subset"
)]
struct Cli {
  /// Select which preset suite to run.
  #[arg(long, value_enum, default_value_t = SuitePreset::Curated)]
  suite: SuitePreset,

  /// Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  manifest: PathBuf,

  /// WPT DOM corpus root (defaults to tests/wpt_dom).
  #[arg(long, value_name = "DIR", default_value = DEFAULT_WPT_ROOT)]
  wpt_root: PathBuf,

  /// Run only a deterministic shard of the selected test list (index/total, 0-based).
  #[arg(long)]
  shard: Option<Shard>,

  /// Custom test selector (glob or regex) applied after the suite preset.
  #[arg(long, value_name = "PATTERN")]
  filter: Option<String>,

  /// Default per-test timeout (seconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  timeout_secs: u64,

  /// Per-test timeout for `timeout=long` tests (seconds).
  #[arg(long, default_value_t = DEFAULT_LONG_TIMEOUT_SECS, value_name = "SECS")]
  long_timeout_secs: u64,

  /// Control which mismatches cause a non-zero exit code.
  #[arg(long, default_value_t = FailOn::New, value_enum)]
  fail_on: FailOn,

  /// JSON report output path.
  #[arg(
    long,
    visible_alias = "report-path",
    value_name = "PATH",
    default_value = DEFAULT_REPORT_PATH
  )]
  report: PathBuf,

  /// Select which JS backend to use.
  #[arg(long, value_enum, default_value_t = Backend::Auto)]
  backend: Backend,
}

fn main() -> Result<()> {
  let cli = Cli::parse();

  if cli.timeout_secs == 0 {
    bail!("--timeout-secs must be > 0");
  }
  if cli.long_timeout_secs == 0 {
    bail!("--long-timeout-secs must be > 0");
  }

  if cli.backend != Backend::Auto {
    // The runner reads this env var only when `BackendSelection::Auto` is used. Set it anyway so
    // callers can force a backend from the CLI without needing to plumb another config layer.
    std::env::set_var(BACKEND_ENV_VAR, cli.backend.as_env_value());
  }

  let filter = cli.filter.or_else(|| cli.suite.default_filter().map(|raw| raw.to_string()));

  let report = run_suite(&SuiteConfig {
    wpt_root: cli.wpt_root.clone(),
    manifest_path: cli.manifest.clone(),
    shard: cli.shard,
    filter,
    timeout: Duration::from_secs(cli.timeout_secs),
    long_timeout: Duration::from_secs(cli.long_timeout_secs),
    fail_on: cli.fail_on,
    backend: BackendSelection::Auto,
  })?;

  if let Some(parent) = cli.report.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }
  let json = serde_json::to_string_pretty(&report).context("serialize report")?;
  fs::write(&cli.report, json).with_context(|| format!("write report {}", cli.report.display()))?;

  println!("WPT DOM report written to {}", cli.report.display());
  println!(
    "Summary: total={} passed={} failed={} timed_out={} errored={} skipped={}",
    report.summary.total,
    report.summary.passed,
    report.summary.failed,
    report.summary.timed_out,
    report.summary.errored,
    report.summary.skipped
  );
  if let Some(mismatches) = &report.summary.mismatches {
    println!(
      "Mismatches: expected={} unexpected={} flaky={} (fail_on={:?})",
      mismatches.expected, mismatches.unexpected, mismatches.flaky, cli.fail_on
    );
  }

  if report.summary.should_fail(cli.fail_on) {
    bail!(
      "WPT DOM suite contains mismatches that violate --fail-on={:?}; see {}",
      cli.fail_on,
      cli.report.display()
    );
  }

  Ok(())
}
