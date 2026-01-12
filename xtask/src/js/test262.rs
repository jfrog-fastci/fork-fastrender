use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
pub use conformance_harness::FailOn;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_TEST262_DIR: &str = "vendor/ecma-rs/test262-semantic/data";
const DEFAULT_REPORT_PATH: &str = "target/js/test262.json";
const DEFAULT_MANIFEST_PATH: &str = "tests/js/test262_manifest.toml";
const DEFAULT_CURATED_SUITE_PATH: &str = "tests/js/test262_suites/curated.toml";
const DEFAULT_SMOKE_SUITE_PATH: &str = "tests/js/test262_suites/smoke.toml";

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_JOBS_CAP: usize = 4;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum HarnessMode {
  /// Prepend the standard tc39/test262 harness (`assert.js`, `sta.js`) plus any additional
  /// frontmatter `includes` files.
  #[value(aliases = ["upstream", "full"])]
  Test262,
  /// Prepend only the harness files explicitly listed in test frontmatter (`includes`).
  ///
  /// This is useful when you want to avoid implicitly loading `assert.js`/`sta.js`.
  ///
  /// Alias: `minimal` (kept for backwards compatibility with older FastRender docs/CLI).
  #[value(aliases = ["minimal"])]
  Includes,
  /// Do not prepend any harness code (test body only).
  None,
}

impl HarnessMode {
  fn as_cli_value(self) -> &'static str {
    match self {
      Self::Test262 => "test262",
      Self::Includes => "includes",
      Self::None => "none",
    }
  }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Test262Suite {
  /// Default curated suite (CI-friendly, deterministic subset).
  Curated,
  /// Minimal suite intended for quick wiring/smoke checks.
  Smoke,
}

#[derive(Args, Debug)]
pub struct Test262Args {
  /// Select which preset suite to run.
  #[arg(long, value_enum, default_value_t = Test262Suite::Curated)]
  pub suite: Test262Suite,

  /// Configure which tc39/test262 harness scripts are prepended before each test body.
  ///
  /// FastRender defaults to `test262`, matching the upstream runner default.
  #[arg(long, value_enum, default_value_t = HarnessMode::Test262)]
  pub harness: HarnessMode,

  /// Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,

  /// Run only a deterministic shard of the corpus (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,

  /// Per-test timeout (seconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  pub timeout_secs: u64,

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

  /// Glob or regex to filter tests by id (after suite selection).
  #[arg(long, value_name = "FILTER")]
  pub filter: Option<String>,

  /// Path to a local checkout of the tc39/test262 repository.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_TEST262_DIR)]
  pub test262_dir: PathBuf,

  /// Extra arguments forwarded to the ecma-rs `test262-semantic` runner (use `--` before these).
  #[arg(last = true)]
  pub extra: Vec<String>,
}

pub fn run_test262(args: Test262Args) -> Result<()> {
  if args.timeout_secs == 0 {
    bail!("--timeout-secs must be > 0");
  }

  let repo_root = crate::repo_root();
  let ecma_rs_root = repo_root.join("vendor/ecma-rs");
  if !ecma_rs_root.join("Cargo.toml").is_file() {
    bail!(
      "Missing vendor/ecma-rs (expected {}).",
      ecma_rs_root.join("Cargo.toml").display()
    );
  }

  let test262_dir = resolve_repo_path(&repo_root, &args.test262_dir);
  ensure_test262_dir(&repo_root, &test262_dir, args.harness)?;

  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  if !manifest_path.is_file() {
    bail!(
      "expectations manifest {} does not exist",
      manifest_path.display()
    );
  }

  let suite_path = repo_root.join(match args.suite {
    Test262Suite::Curated => DEFAULT_CURATED_SUITE_PATH,
    Test262Suite::Smoke => DEFAULT_SMOKE_SUITE_PATH,
  });
  if !suite_path.is_file() {
    bail!("suite file {} does not exist", suite_path.display());
  }

  let report_path = resolve_repo_path(&repo_root, &args.report);
  if let Some(parent) = report_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }

  let jobs = crate::cpu_budget().min(DEFAULT_JOBS_CAP).max(1);
  let shard_arg = args.shard.map(|(idx, total)| format!("{idx}/{total}"));
  let fail_on_arg = match args.fail_on {
    FailOn::All => "all",
    FailOn::New => "new",
    FailOn::None => "none",
  };

  let mut cmd = xtask::cmd::cargo_agent_command(&repo_root);
  cmd
    .arg("run")
    .arg("--release")
    .args(["-p", "test262-semantic"])
    .arg("--")
    .arg("--test262-dir")
    .arg(&test262_dir)
    .arg("--harness")
    .arg(args.harness.as_cli_value())
    .arg("--suite-path")
    .arg(&suite_path)
    .arg("--manifest")
    .arg(&manifest_path)
    .arg("--timeout-secs")
    .arg(args.timeout_secs.to_string())
    .arg("--jobs")
    .arg(jobs.to_string())
    .arg("--report-path")
    .arg(&report_path)
    .arg("--fail-on")
    .arg(fail_on_arg);

  if let Some(shard) = shard_arg {
    cmd.arg("--shard").arg(shard);
  }

  if let Some(filter) = args.filter.as_ref() {
    cmd.arg("--filter").arg(filter);
  }
  if !args.extra.is_empty() {
    cmd.args(&args.extra);
  }

  cmd.current_dir(&ecma_rs_root);
  println!("Running test262 semantic suite ({:?})...", args.suite);
  crate::run_command(cmd)
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn ensure_test262_dir(
  repo_root: &Path,
  test262_dir: &Path,
  harness_mode: HarnessMode,
) -> Result<()> {
  let test_dir = test262_dir.join("test");
  let harness_dir = test262_dir.join("harness");
  let harness_required = !matches!(harness_mode, HarnessMode::None);
  if test_dir.is_dir() && (!harness_required || harness_dir.is_dir()) {
    return Ok(());
  }

  let default_dir = repo_root.join(DEFAULT_TEST262_DIR);
  if test262_dir == default_dir {
    bail!(
      "test262 semantic corpus is missing at {}.\n\
       Initialize it with:\n\
         git submodule update --init vendor/ecma-rs/test262-semantic/data\n\
       \n\
       See docs/js_test262.md for the full workflow.",
      test262_dir.display()
    );
  }

  if harness_required {
    bail!(
      "test262 checkout directory {} is missing required folders (expected {}/test and {}/harness)",
      test262_dir.display(),
      test262_dir.display(),
      test262_dir.display()
    );
  }

  bail!(
    "test262 checkout directory {} is missing required folder (expected {}/test)",
    test262_dir.display(),
    test262_dir.display()
  );
}
