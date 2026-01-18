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

const CHECK_FASTRENDER_LIB: &[&str] = &["check", "-p", "fastrender", "--lib", "--locked"];

const CHECK_FASTRENDER_MINIMAL_LIB: &[&str] = &[
  "check",
  "-p",
  "fastrender",
  "--no-default-features",
  "--features",
  "renderer_minimal",
  "--lib",
  "--locked",
];

fn rustflags_allow_warnings(mut rustflags: String) -> String {
  if !rustflags.contains("-Awarnings") {
    if !rustflags.trim().is_empty() {
      rustflags.push(' ');
    }
    rustflags.push_str("-Awarnings");
  }
  rustflags
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
  cmd.env(
    "RUSTFLAGS",
    rustflags_allow_warnings(std::env::var("RUSTFLAGS").unwrap_or_default()),
  );

  let status = cmd
    .status()
    .with_context(|| format!("failed to run {:?}", cmd))?;
  if !status.success() {
    bail!("command failed with status {status}: {:?}", cmd);
  }
  Ok(())
}

pub fn run_check_lib_builds(repo_root: &Path, args: CheckLibBuildsArgs) -> Result<()> {
  println!("• cargo check -p fastrender --lib --locked");
  run_cargo(repo_root, CHECK_FASTRENDER_LIB, args.quiet)?;

  println!("• cargo check -p fastrender --no-default-features --features renderer_minimal --lib --locked");
  run_cargo(repo_root, CHECK_FASTRENDER_MINIMAL_LIB, args.quiet)?;

  println!("✓ check-lib-builds: ok");
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn check_lib_builds_uses_locked() {
    assert!(
      CHECK_FASTRENDER_LIB.contains(&"--locked"),
      "default lib check should use --locked"
    );
    assert!(
      CHECK_FASTRENDER_MINIMAL_LIB.contains(&"--locked"),
      "minimal lib check should use --locked"
    );
  }

  #[test]
  fn allow_warnings_rustflags_appends_without_clobbering() {
    assert_eq!(rustflags_allow_warnings(String::new()), "-Awarnings");
    assert_eq!(
      rustflags_allow_warnings("-C debuginfo=1".to_string()),
      "-C debuginfo=1 -Awarnings"
    );
    assert_eq!(
      rustflags_allow_warnings("-C debuginfo=1 -Awarnings".to_string()),
      "-C debuginfo=1 -Awarnings"
    );
  }
}
