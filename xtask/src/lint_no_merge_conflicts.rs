use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::Path;
use std::process::Command;

/// Fail if tracked Rust sources contain unresolved git merge-conflict markers.
///
/// This is primarily a CI/automation guardrail: merge-conflict markers in `.rs` sources can break
/// compilation in surprising ways and are easy to miss in large diffs.
#[derive(Args, Debug, Clone, Copy)]
pub struct LintNoMergeConflictsArgs {}

pub fn run_lint_no_merge_conflicts(repo_root: &Path, _args: LintNoMergeConflictsArgs) -> Result<()> {
  let mut cmd = Command::new("git");
  cmd.current_dir(repo_root);
  cmd.args([
    "-c",
    "grep.recurseSubmodules=false",
    "grep",
    "-n",
    "-I",
    "-e",
    "^<<<<<<< ",
    "-e",
    "^||||||| ",
    "-e",
    "^=======[[:space:]]*$",
    "-e",
    "^>>>>>>> ",
    "--",
    "*.rs",
    "*.toml",
  ]);

  let output = cmd
    .output()
    .with_context(|| format!("failed to execute `{:?}`", cmd))?;

  match output.status.code() {
    Some(0) => {
      let stdout = String::from_utf8_lossy(&output.stdout);
      let stderr = String::from_utf8_lossy(&output.stderr);
      let mut details = String::new();
      if !stdout.trim().is_empty() {
        details.push_str(stdout.trim_end());
        details.push('\n');
      }
      if !stderr.trim().is_empty() {
        details.push_str(stderr.trim_end());
        details.push('\n');
      }

      bail!(
        "lint-no-merge-conflicts: found unresolved git merge-conflict markers in tracked Rust sources:\n{details}\n\
         hint: resolve the conflict and delete the <<<<<<< / ||||||| / ======= / >>>>>>> lines before committing."
      );
    }
    Some(1) => {
      println!("✓ lint-no-merge-conflicts: no merge-conflict markers found in tracked *.rs/*.toml files");
      Ok(())
    }
    _ => {
      bail!(
        "lint-no-merge-conflicts: `git grep` failed with status {}.\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }
}
