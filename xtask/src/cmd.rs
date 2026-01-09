use std::path::Path;
use std::process::Command;

/// Default address-space limit used by `scripts/run_limited.sh`.
///
/// `scripts/cargo_agent.sh` defaults to `64G` as well, but xtask also executes renderer binaries
/// directly in a few workflows (e.g. to preserve exit codes). Those invocations must still be
/// capped.
pub const DEFAULT_LIMIT_AS: &str = "64G";

/// Construct a `Command` that runs cargo through the agent-safe wrapper:
///
/// ```text
/// bash <repo_root>/scripts/cargo_agent.sh <subcommand> ...
/// ```
pub fn cargo_agent_command(repo_root: &Path) -> Command {
  let mut cmd = Command::new("bash");
  cmd.arg(repo_root.join("scripts/cargo_agent.sh"));
  cmd
}

/// Construct a `Command` that executes another command under OS-enforced resource limits:
///
/// ```text
/// bash <repo_root>/scripts/run_limited.sh --as <limit> -- <cmd...>
/// ```
pub fn run_limited_command(repo_root: &Path, as_limit: &str) -> Command {
  let mut cmd = Command::new("bash");
  cmd
    .arg(repo_root.join("scripts/run_limited.sh"))
    .args(["--as", as_limit, "--"]);
  cmd
}

/// Convenience wrapper for `run_limited_command` using [`DEFAULT_LIMIT_AS`].
pub fn run_limited_command_default(repo_root: &Path) -> Command {
  run_limited_command(repo_root, DEFAULT_LIMIT_AS)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .expect("xtask crate should live under the repository root")
      .to_path_buf()
  }

  #[test]
  fn cargo_agent_command_uses_bash_wrapper() {
    let cmd = cargo_agent_command(&repo_root());
    assert_eq!(cmd.get_program().to_string_lossy(), "bash");
    let args: Vec<String> = cmd
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect();
    assert!(
      args.iter().any(|arg| arg.ends_with("scripts/cargo_agent.sh")),
      "expected command args to include scripts/cargo_agent.sh; got {args:?}"
    );
  }

  #[test]
  fn run_limited_command_uses_bash_wrapper() {
    let cmd = run_limited_command(&repo_root(), "1G");
    assert_eq!(cmd.get_program().to_string_lossy(), "bash");
    let args: Vec<String> = cmd
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect();
    assert!(
      args.iter().any(|arg| arg.ends_with("scripts/run_limited.sh")),
      "expected command args to include scripts/run_limited.sh; got {args:?}"
    );
    assert!(
      args.windows(2).any(|w| w == ["--as", "1G"]),
      "expected command args to include --as 1G; got {args:?}"
    );
    assert!(
      args.iter().any(|arg| arg == "--"),
      "expected command args to include `--`; got {args:?}"
    );
  }
}

