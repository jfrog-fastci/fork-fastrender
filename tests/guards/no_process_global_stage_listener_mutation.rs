//! Guard against mutating the process-global render stage listener directly in the unified
//! integration test binary.
//!
//! After consolidating integration tests into a single binary (`tests/integration.rs`), the global
//! stage listener is shared across all tests running in the same process. Tests should install a
//! listener via `crate::common::StageListenerGuard` so installs are coordinated and automatically
//! restored.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_mutate_process_stage_listener_directly() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let allow_mutation_file = tests_root.join("common").join("global_state.rs");

  // Build these strings indirectly so this guard test itself doesn't trip the substring checks.
  let render_control = "render_control::";
  let global = "Global";
  let stage_guard = "StageListenerGuard";
  let open_paren = "(";
  let forbidden = [
    format!("{render_control}set_stage_listener{open_paren}"),
    format!("{render_control}swap_stage_listener{open_paren}"),
    // Forbid using the library guard directly; use `crate::common::StageListenerGuard` instead.
    format!("{global}{stage_guard}"),
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
    if path == allow_mutation_file {
      continue;
    }

    let contents = fs::read_to_string(path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch `render_control :: set_stage_listener` variations too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; use `crate::common::StageListenerGuard` instead",
        path.display(),
        needle
      );
    }
  }
}
