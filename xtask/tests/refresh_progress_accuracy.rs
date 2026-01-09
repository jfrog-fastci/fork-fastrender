use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

#[test]
fn refresh_progress_accuracy_dry_run_prints_plan_and_wires_progress_dir() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress");
  let out_dir = temp.path().join("out");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "refresh-progress-accuracy",
      "--dry-run",
      "--progress-dir",
      progress_dir.to_string_lossy().as_ref(),
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
      "--fixtures",
      "example_fixture",
    ])
    .output()
    .expect("run xtask refresh-progress-accuracy --dry-run");

  assert!(
    output.status.success(),
    "expected dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("refresh-progress-accuracy plan:")
      && stdout.contains(progress_dir.to_string_lossy().as_ref())
      && stdout.contains(out_dir.to_string_lossy().as_ref())
      && stdout.contains("sync-progress-accuracy --report")
      && stdout.contains("--progress-dir"),
    "expected dry-run output to include a plan and the progress dir wiring; got:\n{stdout}"
  );
}

