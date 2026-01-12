//! Guard against directly mutating process-global environment/current-dir state in the unified
//! integration test binary.
//!
//! After consolidating integration tests into a single binary (`tests/integration.rs`), calls like
//! `std::env::set_var` can leak across unrelated tests. Mutations should go through the shared
//! RAII helpers in `tests/common/global_state.rs` so they are serialized and automatically restored.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_mutate_process_env_or_cwd_directly() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let allow_mutation_file = tests_root.join("common").join("global_state.rs");

  // Build these strings indirectly so this guard test itself doesn't trip the substring checks.
  let std_env = "std::env::";
  let env = "env::";
  let forbidden = [
    format!("{std_env}set_var("),
    format!("{std_env}remove_var("),
    format!("{std_env}set_current_dir("),
    format!("{env}set_var("),
    format!("{env}remove_var("),
    format!("{env}set_current_dir("),
  ];

  for entry in WalkDir::new(&tests_root) {
    let entry = entry.unwrap_or_else(|err| panic!("walk tests dir: {err}"));
    let path = entry.path();
    if !is_rust_file(path) {
      continue;
    }
    if path == allow_mutation_file {
      continue;
    }

    let contents =
      fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch `std::env::set_var (\"X\"` variations too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; use `tests/common/global_state.rs` helpers instead",
        path.display(),
        needle
      );
    }
  }
}

