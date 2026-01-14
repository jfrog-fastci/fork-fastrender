use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::Path;
use std::process::Command;

/// Run fast, renderer-only compile checks for the `fastrender` library.
///
/// This is a lightweight guardrail intended to catch build breaks that only show up when:
/// - compiling the core library (`--lib`), and/or
/// - compiling with the minimal renderer feature set (`--no-default-features --features renderer_minimal`).
#[derive(Args, Debug, Clone, Copy)]
pub struct CheckLibBuildsArgs {
  /// Forward `--quiet` to cargo to reduce output noise.
  #[arg(long)]
  pub quiet: bool,
}

fn run_cargo(repo_root: &Path, args: &[&str], quiet: bool) -> Result<()> {
  let mut cmd: Command = crate::cmd::cargo_agent_command(repo_root);
  cmd.current_dir(repo_root);
  cmd.args(args);
  if quiet {
    cmd.arg("--quiet");
  }
  // xtask generally keeps compilation noise low; use the same convention as many CI verification
  // snippets in this repo.
  cmd.env("RUSTFLAGS", "-Awarnings");

  let status = cmd
    .status()
    .with_context(|| format!("failed to run {:?}", cmd))?;
  if !status.success() {
    bail!("command failed with status {status}: {:?}", cmd);
  }
  Ok(())
}

pub fn run_check_lib_builds(repo_root: &Path, args: CheckLibBuildsArgs) -> Result<()> {
  println!("• cargo check -p fastrender --lib");
  run_cargo(repo_root, &["check", "-p", "fastrender", "--lib"], args.quiet)?;

  println!("• cargo check -p fastrender --no-default-features --features renderer_minimal --lib");
  run_cargo(
    repo_root,
    &[
      "check",
      "-p",
      "fastrender",
      "--no-default-features",
      "--features",
      "renderer_minimal",
      "--lib",
    ],
    args.quiet,
  )?;

  println!("✓ check-lib-builds: ok");
  Ok(())
}

