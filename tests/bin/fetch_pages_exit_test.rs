use std::process::Command;
use tempfile::TempDir;

#[test]
fn fetch_pages_exits_non_zero_when_filter_matches_nothing() {
  let temp = TempDir::new().expect("tempdir");

  let status = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .current_dir(temp.path())
    .args(["--pages", "definitely-not-here"])
    .status()
    .expect("run fetch_pages");

  assert_eq!(
    status.code(),
    Some(1),
    "expected failure exit code when filter matches nothing"
  );
}

#[test]
fn fetch_pages_errors_on_unknown_option() {
  let temp = TempDir::new().expect("tempdir");

  let status = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .current_dir(temp.path())
    .arg("--definitely-not-a-flag")
    .status()
    .expect("run fetch_pages");

  assert!(
    !status.success(),
    "expected non-zero exit when unknown option is provided (got {:?})",
    status.code()
  );
}

#[test]
fn fetch_pages_rejects_cache_dir_flag() {
  let temp = TempDir::new().expect("tempdir");

  let output = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .current_dir(temp.path())
    .args(["--cache-dir", "ignored", "--pages", "definitely-not-here"])
    .output()
    .expect("run fetch_pages");

  assert_eq!(
    output.status.code(),
    Some(2),
    "expected clap usage error when --cache-dir is provided; stdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

#[test]
fn fetch_pages_rejects_zero_jobs() {
  let temp = TempDir::new().expect("tempdir");

  let output = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .current_dir(temp.path())
    .args(["--jobs", "0", "--pages", "definitely-not-here"])
    .output()
    .expect("run fetch_pages");

  assert_eq!(
    output.status.code(),
    Some(2),
    "expected usage error for --jobs 0; stdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("jobs must be > 0"),
    "expected jobs validation error; got stderr:\n{stderr}"
  );
  assert!(
    !stderr.contains("panicked at"),
    "did not expect panic output; got stderr:\n{stderr}"
  );
}

#[test]
fn fetch_pages_exits_cleanly_when_cache_dir_creation_fails() {
  let temp = TempDir::new().expect("tempdir");

  // Prevent `fetch_pages` from creating `fetches/html` by placing a file at `fetches`.
  std::fs::write(temp.path().join("fetches"), "not a directory").expect("write blocker");

  let output = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .current_dir(temp.path())
    .args(["--pages", "definitely-not-here"])
    .output()
    .expect("run fetch_pages");

  assert_eq!(
    output.status.code(),
    Some(1),
    "expected failure exit code when cache dir creation fails; stdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("Failed to create cache dir"),
    "expected cache dir error; got stderr:\n{stderr}"
  );
  assert!(
    !stderr.contains("panicked at"),
    "did not expect panic output; got stderr:\n{stderr}"
  );
}
