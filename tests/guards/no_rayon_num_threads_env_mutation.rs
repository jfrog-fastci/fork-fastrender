//! Guard against mutating `RAYON_NUM_THREADS` inside the unified integration test process.
//!
//! After consolidating integration tests into a single binary (`tests/integration.rs`), the Rayon
//! global thread-pool becomes process-global and irreversible. Tests must not attempt to change the
//! number of Rayon threads via env var mutation; use per-renderer configuration instead (see
//! `common::init_rayon_for_tests`).

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_mutate_rayon_num_threads_env_var() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let var = "RAYON_NUM_THREADS";
  let forbidden = [
    format!("set_var({var:?}"),
    format!("remove_var({var:?}"),
    format!("EnvVarGuard::set({var:?}"),
    format!("EnvVarGuard::remove({var:?}"),
    format!("ScopedEnv::set({var:?}"),
    format!("ScopedEnv::remove({var:?}"),
    format!("EnvVarsGuard::new(&[({var:?},"),
    format!("EnvVarsGuard::set(&[({var:?},"),
    format!("EnvVarsGuard::remove(&[{var:?}]"),
    // Convenience helper that uses EnvVarsGuard internally.
    format!("with_env_vars(&[({var:?},"),
  ];

  for entry in WalkDir::new(&tests_root)
    .into_iter()
    .filter_entry(|entry| !super::should_skip_tests_entry(entry, &tests_root))
  {
    let entry = entry.unwrap_or_else(|err| panic!("walk tests dir: {err}"));
    let path = entry.path();
    if !is_rust_file(path) {
      continue;
    }
    let contents = fs::read_to_string(path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch whitespace variations (e.g. with spaces in the call) too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; the Rayon global pool is process-global and irreversible",
        path.display(),
        needle
      );
    }
  }
}
