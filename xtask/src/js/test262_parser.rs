use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
 
use super::test262::FailOn;
 
const DEFAULT_TEST262_DIR: &str = "engines/ecma-rs/test262/data";
const DEFAULT_REPORT_PATH: &str = "target/js/test262-parser.json";
const DEFAULT_MANIFEST_PATH: &str = "tests/js/test262_parser_expectations.toml";
 
#[derive(Args, Debug)]
pub struct Test262ParserArgs {
  /// Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,
 
  /// Run only a deterministic shard of the corpus (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,
 
  /// Per-test timeout (seconds).
  #[arg(long, value_name = "SECS")]
  pub timeout_secs: Option<u64>,
 
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
 
  /// Path to a local checkout of the tc39/test262-parser-tests corpus (nested submodule inside ecma-rs).
  #[arg(long, value_name = "DIR", default_value = DEFAULT_TEST262_DIR)]
  pub test262_dir: PathBuf,
}
 
pub fn run_test262_parser(args: Test262ParserArgs) -> Result<()> {
  let repo_root = crate::repo_root();
  let ecma_rs_root = repo_root.join("engines/ecma-rs");
  if !ecma_rs_root.join("Cargo.toml").is_file() {
    bail!(
      "Missing engines/ecma-rs submodule checkout (expected {}).\n\
       Run:\n\
         git submodule update --init engines/ecma-rs",
      ecma_rs_root.join("Cargo.toml").display()
    );
  }
 
  let test262_dir = resolve_repo_path(&repo_root, &args.test262_dir);
  ensure_test262_dir(&repo_root, &test262_dir)?;
 
  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  if !manifest_path.is_file() {
    bail!(
      "expectations manifest {} does not exist",
      manifest_path.display()
    );
  }
 
  let report_path = resolve_repo_path(&repo_root, &args.report);
  if let Some(parent) = report_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }
 
  let shard_arg = args.shard.map(|(idx, total)| format!("{idx}/{total}"));
 
  let mut cmd = Command::new("cargo");
  cmd
    .arg("run")
    .arg("--release")
    .args(["-p", "test262"])
    .arg("--")
    .arg("--data-dir")
    .arg(&test262_dir)
    .arg("--manifest")
    .arg(&manifest_path)
    .arg("--report-path")
    .arg(&report_path)
    .arg("--fail-on")
    .arg(args.fail_on.as_cli_value());
 
  if let Some(shard) = shard_arg {
    cmd.arg("--shard").arg(shard);
  }
  if let Some(timeout) = args.timeout_secs {
    if timeout == 0 {
      bail!("--timeout-secs must be > 0");
    }
    cmd.arg("--timeout-secs").arg(timeout.to_string());
  }
 
  cmd.current_dir(&ecma_rs_root);
  println!("Running test262 parser harness...");
  crate::run_command(cmd)
}
 
fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}
 
fn ensure_test262_dir(repo_root: &Path, test262_dir: &Path) -> Result<()> {
  let test_dir = test262_dir.join("pass");
  if test_dir.is_dir() {
    return Ok(());
  }
 
  let default_dir = repo_root.join(DEFAULT_TEST262_DIR);
  if test262_dir == default_dir {
    bail!(
      "test262 parser corpus is missing at {}.\n\
       This is a nested submodule; initialize it with:\n\
         git -C engines/ecma-rs submodule update --init test262/data\n\
       \n\
       See docs/js_test262_parser.md for the full workflow.",
      test262_dir.display()
    );
  }
 
  bail!(
    "test262 parser checkout directory {} is missing expected folders (expected {}/pass)",
    test262_dir.display(),
    test262_dir.display()
  );
}

