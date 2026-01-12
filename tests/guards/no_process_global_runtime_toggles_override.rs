//! Guard against mutating process-global runtime toggles inside the unified integration test binary.
//!
//! `fastrender::debug::runtime::{set_runtime_toggles, with_runtime_toggles}` updates a
//! process-global override (protected by locks), which can still leak across concurrently executing
//! tests under libtest's default parallelism.
//!
//! Integration tests should prefer:
//! - passing `RuntimeToggles` explicitly via `FastRenderConfig` / `RenderOptions`, or
//! - using thread-local overrides (`set_thread_runtime_toggles` / `with_thread_runtime_toggles`)
//!   when a function reads `runtime_toggles()` directly.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_install_global_runtime_toggles_overrides() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let allow_file = tests_root
    .join("guards")
    .join("no_process_global_runtime_toggles_override.rs");

  // Build these strings indirectly so this guard test itself doesn't trip the substring checks.
  let use_kw = "use";
  let runtime_mod = "fastrender::debug::runtime::";
  let brace_import = format!("{use_kw}{runtime_mod}{{");
  let forbidden = [
    // Direct path call-sites.
    format!("{runtime_mod}set_runtime_toggles("),
    format!("{runtime_mod}with_runtime_toggles("),
    // Imports that would allow unqualified calls.
    format!("{brace_import}set_runtime_toggles"),
    format!("{brace_import}with_runtime_toggles"),
    // Unqualified calls after import. (`with_runtime_toggles(` would also match the builder method
    // `.with_runtime_toggles(`, so only check the unique `set_runtime_toggles` token here.)
    "set_runtime_toggles(".to_string(),
  ];

  for entry in WalkDir::new(&tests_root) {
    let entry = entry.unwrap_or_else(|err| panic!("walk tests dir: {err}"));
    let path = entry.path();
    if !is_rust_file(path) {
      continue;
    }
    if path == allow_file {
      continue;
    }

    let contents = fs::read_to_string(path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch `use fastrender :: debug :: runtime :: set_runtime_toggles` too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; avoid process-global runtime toggles overrides in integration tests",
        path.display(),
        needle
      );
    }
  }
}

