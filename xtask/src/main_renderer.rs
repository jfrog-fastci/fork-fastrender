#![recursion_limit = "256"]

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;

mod chrome_baseline_fixtures;
mod cmd;
mod fixture_chrome_diff;
mod fixture_html_patch;
mod pageset_failure_fixtures;
mod refresh_progress_accuracy;
mod sync_progress_accuracy;

#[derive(Parser, Debug)]
#[command(name = "xtask", version, about)]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Render offline fixtures with both FastRender and headless Chrome, then diff.
  FixtureChromeDiff(fixture_chrome_diff::FixtureChromeDiffArgs),
  /// Render offline fixtures in headless Chrome and write PNG screenshots.
  ChromeBaselineFixtures(chrome_baseline_fixtures::ChromeBaselineFixturesArgs),
  /// Sync `accuracy` metrics from a `diff_renders` JSON report into `progress/pages/*.json`.
  SyncProgressAccuracy(sync_progress_accuracy::SyncProgressAccuracyArgs),
  /// Run fixture-chrome-diff and sync progress accuracy metrics (pageset baseline).
  RefreshProgressAccuracy(refresh_progress_accuracy::RefreshProgressAccuracyArgs),
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  match cli.command {
    Commands::FixtureChromeDiff(args) => fixture_chrome_diff::run_fixture_chrome_diff(args),
    Commands::ChromeBaselineFixtures(args) => {
      chrome_baseline_fixtures::run_chrome_baseline_fixtures(args)
    }
    Commands::SyncProgressAccuracy(args) => {
      sync_progress_accuracy::run_sync_progress_accuracy(args)
    }
    Commands::RefreshProgressAccuracy(args) => {
      refresh_progress_accuracy::run_refresh_progress_accuracy(args)
    }
  }
}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask manifest should be in repository root")
    .to_path_buf()
}

fn resolve_cargo_target_dir(repo_root: &Path, cargo_target_dir: Option<&Path>) -> PathBuf {
  match cargo_target_dir {
    Some(path) if path.as_os_str().is_empty() => repo_root.join("target"),
    Some(path) if path.is_absolute() => path.to_path_buf(),
    Some(path) => repo_root.join(path),
    None => repo_root.join("target"),
  }
}

fn cargo_target_dir(repo_root: &Path) -> PathBuf {
  let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
  resolve_cargo_target_dir(repo_root, cargo_target_dir.as_deref())
}

fn parse_viewport(raw: &str) -> Result<(u32, u32)> {
  let (width, height) = raw
    .split_once('x')
    .ok_or_else(|| anyhow!("viewport must be formatted as WxH"))?;

  let width = width
    .parse::<u32>()
    .context("failed to parse viewport width")?;
  let height = height
    .parse::<u32>()
    .context("failed to parse viewport height")?;

  if width == 0 || height == 0 {
    bail!("viewport dimensions must be greater than zero");
  }

  Ok((width, height))
}

fn parse_shard(s: &str) -> Result<(usize, usize), String> {
  let parts: Vec<&str> = s.split('/').collect();
  if parts.len() != 2 {
    return Err("shard must be index/total (e.g., 0/4)".to_string());
  }
  let index = parts[0]
    .parse::<usize>()
    .map_err(|_| "invalid shard index".to_string())?;
  let total = parts[1]
    .parse::<usize>()
    .map_err(|_| "invalid shard total".to_string())?;
  if total == 0 {
    return Err("shard total must be > 0".to_string());
  }
  if index >= total {
    return Err("shard index must be < total".to_string());
  }
  Ok((index, total))
}

fn run_command(mut cmd: Command) -> Result<()> {
  print_command(&cmd);

  let status = cmd
    .status()
    .with_context(|| format!("failed to run {:?}", cmd.get_program()))?;
  if !status.success() {
    bail!("command failed with status {status}");
  }
  Ok(())
}

fn print_command(cmd: &Command) {
  let envs = cmd
    .get_envs()
    .filter_map(|(k, v)| v.map(|v| format!("{}={}", k.to_string_lossy(), v.to_string_lossy())))
    .collect::<Vec<_>>();

  if !envs.is_empty() {
    print!("$ {} ", envs.join(" "));
  } else {
    print!("$ ");
  }

  print!("{}", cmd.get_program().to_string_lossy());
  for arg in cmd.get_args() {
    print!(" {}", arg.to_string_lossy());
  }
  println!();
}

