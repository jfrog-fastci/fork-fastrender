//! Smoke-test the `scripts/ci_check_no_merge_conflicts.sh` guard script.
//!
//! The Rust-only guard (`no_merge_markers.rs`) ensures we don't accidentally commit conflict markers
//! into Rust sources, but the script is repo-wide and runs in CI. Keep a small focused test here so
//! the script's behavior (exit codes + diagnostics) can't silently regress.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn merge_conflict_marker_script_reports_offenders() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let script = repo_root.join("scripts/ci_check_no_merge_conflicts.sh");

  // `cargo test` runs on Windows/macOS/Linux in CI. The repository already relies on bash for
  // multiple CI guardrail scripts, but keep this test non-fatal if a local environment is missing
  // bash (e.g. minimal Windows without Git Bash).
  if Command::new("bash").arg("--version").output().is_err() {
    eprintln!("bash not available; skipping conflict-marker guard script test");
    return;
  }

  let dir = tempdir().expect("create tempdir for conflict marker guard script test");

  // Clean tree should pass.
  let clean = Command::new("bash")
    .arg(&script)
    .arg("--path")
    .arg(dir.path())
    .output()
    .expect("run ci_check_no_merge_conflicts.sh on empty dir");
  assert!(
    clean.status.success(),
    "expected clean scan to succeed; status={:?}\nstderr:\n{}\nstdout:\n{}",
    clean.status.code(),
    String::from_utf8_lossy(&clean.stderr),
    String::from_utf8_lossy(&clean.stdout)
  );

  // A directory containing conflict markers should fail and print file:line diagnostics.
  fs::write(
    dir.path().join("bad.txt"),
    "<<<<<<< HEAD\nleft\n=======\nright\n>>>>>>> branch\n",
  )
  .expect("write bad.txt fixture");
  fs::write(dir.path().join("good.txt"), "hello world\n").expect("write good.txt fixture");

  let dirty = Command::new("bash")
    .arg(&script)
    .arg("--path")
    .arg(dir.path())
    .output()
    .expect("run ci_check_no_merge_conflicts.sh on dir containing markers");

  assert!(
    !dirty.status.success(),
    "expected conflict marker scan to fail; status={:?}\nstderr:\n{}\nstdout:\n{}",
    dirty.status.code(),
    String::from_utf8_lossy(&dirty.stderr),
    String::from_utf8_lossy(&dirty.stdout)
  );

  let stderr = String::from_utf8_lossy(&dirty.stderr);
  assert!(
    stderr.contains("bad.txt:1:"),
    "expected stderr to include offending file + line number, got:\n{stderr}"
  );
  assert!(
    stderr.contains("bad.txt:3:"),
    "expected stderr to include ======= marker line, got:\n{stderr}"
  );
  assert!(
    stderr.contains("bad.txt:5:"),
    "expected stderr to include >>>>>>> marker line, got:\n{stderr}"
  );
  assert!(
    !stderr.contains("good.txt:"),
    "did not expect stderr to include good.txt (no conflict markers), got:\n{stderr}"
  );
  assert!(
    !stderr.contains(":left"),
    "did not expect stderr to include non-marker lines, got:\n{stderr}"
  );
  assert!(
    !stderr.contains(":right"),
    "did not expect stderr to include non-marker lines, got:\n{stderr}"
  );
}
