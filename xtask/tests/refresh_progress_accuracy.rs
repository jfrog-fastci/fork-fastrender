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
      && stdout.contains("viewport: 1200x800")
      && stdout.contains("timeout: 120s")
      && stdout.contains("sync-progress-accuracy --report")
      && stdout.contains("--progress-dir"),
    "expected dry-run output to include a plan and the progress dir wiring; got:\n{stdout}"
  );
}

#[test]
fn refresh_progress_accuracy_dry_run_forwards_shard_to_fixture_chrome_diff() {
  let temp = tempdir().expect("tempdir");
  let out_dir = temp.path().join("out");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "refresh-progress-accuracy",
      "--dry-run",
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
      "--fixtures",
      "example_fixture",
      "--shard",
      "1/8",
    ])
    .output()
    .expect("run xtask refresh-progress-accuracy --dry-run --shard 1/8");

  assert!(
    output.status.success(),
    "expected dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture-chrome-diff")
      && stdout.contains("--shard 1/8")
      && stdout.contains("viewport: 1200x800"),
    "expected dry-run output to include the forwarded fixture-chrome-diff shard; got:\n{stdout}"
  );
}

#[test]
fn refresh_progress_accuracy_rejects_invalid_shard() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["refresh-progress-accuracy", "--dry-run", "--shard", "8/8"])
    .output()
    .expect("run xtask refresh-progress-accuracy --shard 8/8");

  assert!(
    !output.status.success(),
    "expected invalid shard to fail.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("shard index must be < total"),
    "expected shard parse error; got stderr:\n{stderr}"
  );
}
