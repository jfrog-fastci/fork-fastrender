//! Guard against mutating `PATH` inside the unified integration test process.
//!
//! After consolidating integration tests into a single binary (`tests/integration.rs`), process-wide
//! env var mutations can leak across unrelated tests. Mutating `PATH` is especially risky because
//! it affects every child process spawned by other tests (including Cargo-built helper binaries).
//!
//! If a test needs to simulate "tool not available on PATH", prefer dependency injection, explicit
//! command paths, or env-based fallbacks (`FASTR_GIT_SHA` / `GITHUB_SHA` for commit attribution)
//! rather than mutating `PATH` globally.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_mutate_path_env_var() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let var = "PATH";
  let forbidden = [
    // Process-wide mutations (including through helper RAII guards).
    format!("set_var({var:?},"),
    format!("remove_var({var:?})"),
    format!("EnvVarGuard::set({var:?},"),
    format!("EnvVarGuard::remove({var:?})"),
    format!("EnvVarsGuard::new(&[({var:?},"),
    format!("EnvVarsGuard::set(&[({var:?},"),
    format!("EnvVarsGuard::remove(&[{var:?}]"),
    // Convenience helper that uses EnvVarsGuard internally.
    format!("with_env_vars(&[({var:?},"),
  ];

  for entry in WalkDir::new(&tests_root) {
    let entry = entry.unwrap_or_else(|err| panic!("walk tests dir: {err}"));
    let path = entry.path();
    if !is_rust_file(path) {
      continue;
    }
    let contents = fs::read_to_string(path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch `set_var ( \"PATH\"` variations too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; mutating PATH is process-global and breaks test isolation",
        path.display(),
        needle
      );
    }
  }
}
