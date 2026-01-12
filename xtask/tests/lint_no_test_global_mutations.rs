use regex::Regex;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live one directory below the repo root")
    .to_path_buf()
}

fn should_skip_tests_entry(entry: &DirEntry, tests_root: &Path) -> bool {
  let path = entry.path();
  let rel = match path.strip_prefix(tests_root) {
    Ok(rel) => rel,
    Err(_) => return false,
  };
  let mut components = rel.components();
  let Some(std::path::Component::Normal(first)) = components.next() else {
    return false;
  };

  // `tests/bin/**` contains harness/subprocess tests and is allowed to set process state at startup.
  if first == OsStr::new("bin") {
    return true;
  }

  // Large fixture directories under `tests/` can contain hundreds of thousands of non-Rust files.
  // The lint only cares about `.rs` sources, so skip these to keep `cargo test -p xtask` fast and
  // deterministic.
  if matches!(
    first.to_string_lossy().as_ref(),
    "pages" | "fonts" | "fuzz_corpus" | "wpt_dom"
  ) {
    return true;
  }

  // `tests/wpt/tests/**` and `tests/wpt/expected/**` are HTML/image fixtures (not Rust sources).
  if first == OsStr::new("wpt") {
    if matches!(
      components.next(),
      Some(std::path::Component::Normal(seg))
        if seg == OsStr::new("tests") || seg == OsStr::new("expected")
    ) {
      return true;
    }
  }

  false
}

fn is_allowlisted_source(path: &Path, repo_root: &Path) -> bool {
  let rel = match path.strip_prefix(repo_root) {
    Ok(rel) => rel,
    Err(_) => return false,
  };

  // The unified test suite relies on a centralized guard module for the handful of globals that
  // remain (environment variables, Rayon global pool initialization). The lint enforces that all
  // other test code goes through those guards rather than mutating process globals directly.
  matches!(
    rel.to_string_lossy().as_ref(),
    // Task 154's centralized env-var/global-state guard (name may vary across migrations).
    "tests/common/mod.rs"
      | "tests/common/global_state.rs"
      | "tests/common/global_state/mod.rs"
      // Centralized Rayon global pool initializer.
      | "tests/common/rayon.rs"
      | "tests/common/rayon_test_util.rs"
  )
}

#[test]
fn lint_no_test_global_mutations() {
  let repo_root = repo_root();
  let tests_root = repo_root.join("tests");
  assert!(
    tests_root.is_dir(),
    "expected {} to exist",
    tests_root.display()
  );

  let env_set_var = Regex::new(r"\bstd::env::set_var\s*\(").expect("valid regex");
  let env_remove_var = Regex::new(r"\bstd::env::remove_var\s*\(").expect("valid regex");
  let env_set_current_dir = Regex::new(r"\bstd::env::set_current_dir\s*\(").expect("valid regex");
  let rayon_build_global = Regex::new(r"\bbuild_global\s*\(").expect("valid regex");
  let set_stage_listener = Regex::new(r"\bset_stage_listener\s*\(").expect("valid regex");

  let patterns: &[(&str, &Regex, &str)] = &[
    (
      "std::env::set_var",
      &env_set_var,
      "Do not mutate process environment variables from tests. Use runtime toggles / builder \
       config, or the shared `tests/common` global-state guards (`global_test_lock` + \
       `EnvVarGuard`).",
    ),
    (
      "std::env::remove_var",
      &env_remove_var,
      "Do not mutate process environment variables from tests. Use runtime toggles / builder \
       config, or the shared `tests/common` global-state guards (`global_test_lock` + \
       `EnvVarGuard`).",
    ),
    (
      "std::env::set_current_dir",
      &env_set_current_dir,
      "Do not mutate the process working directory from tests. Prefer absolute paths or pass a \
       base directory explicitly.",
    ),
    (
      "rayon::ThreadPoolBuilder::build_global",
      &rayon_build_global,
      "Do not call `rayon::ThreadPoolBuilder::build_global()` from tests. Initialize Rayon once \
       via `crate::common::init_rayon_for_tests` (implemented in `tests/common/rayon.rs`).",
    ),
    (
      "set_stage_listener",
      &set_stage_listener,
      "Do not call `set_stage_listener()` from tests. Use `GlobalStageListenerGuard` instead.",
    ),
  ];

  let mut violations: Vec<String> = Vec::new();

  for entry in WalkDir::new(&tests_root)
    .into_iter()
    .filter_entry(|entry| !should_skip_tests_entry(entry, &tests_root))
  {
    let entry = entry.unwrap_or_else(|err| panic!("walkdir failed under tests/: {err}"));
    if !entry.file_type().is_file() {
      continue;
    }
    if entry.path().extension() != Some(OsStr::new("rs")) {
      continue;
    }
    if is_allowlisted_source(entry.path(), &repo_root) {
      continue;
    }

    let content = fs::read_to_string(entry.path())
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", entry.path().display()));
    let rel_path = entry
      .path()
      .strip_prefix(&repo_root)
      .unwrap_or(entry.path())
      .display()
      .to_string();

    for (idx, line) in content.lines().enumerate() {
      let line_no = idx + 1;
      for (label, regex, remediation) in patterns {
        if regex.is_match(line) {
          violations.push(format!(
            "{rel_path}:{line_no}: forbidden `{label}` usage. {remediation}"
          ));
        }
      }
    }
  }

  if !violations.is_empty() {
    panic!(
      "Found process-global state mutations in `tests/`.\n\
       The unified integration test suite runs inside a single process, so these APIs can cause \
       test flakes.\n\
       \n\
       Fix by using builder/runtime configuration or the shared guards in `tests/common/`.\n\
       \n\
       Violations:\n\
       {}",
      violations.join("\n")
    );
  }
}
