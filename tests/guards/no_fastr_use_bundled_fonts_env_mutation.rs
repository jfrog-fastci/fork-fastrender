//! Guard against mutating `FASTR_USE_BUNDLED_FONTS` inside the unified integration test process.
//!
//! After consolidating integration tests into a single binary (`tests/integration.rs`), process-wide
//! env var mutations can leak across unrelated tests. Bundled-font selection should be configured
//! per-renderer via `FontConfig::bundled_only()` rather than via `std::env::set_var`.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

fn is_rust_file(path: &Path) -> bool {
  path.is_file() && path.extension() == Some(OsStr::new("rs"))
}

#[test]
fn tests_do_not_mutate_fastr_use_bundled_fonts_env_var() {
  let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let var = "FASTR_USE_BUNDLED_FONTS";
  let forbidden = [
    format!("set_var({var:?}"),
    format!("remove_var({var:?}"),
    format!("EnvVarGuard::set({var:?}"),
    format!("EnvVarGuard::remove({var:?}"),
  ];

  for entry in WalkDir::new(&tests_root) {
    let entry = entry.unwrap_or_else(|err| panic!("walk tests dir: {err}"));
    let path = entry.path();
    if !is_rust_file(path) {
      continue;
    }
    let contents = fs::read_to_string(path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Strip whitespace so we catch `set_var ( \"FASTR_USE_BUNDLED_FONTS\"` variations too.
    let condensed: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    for needle in &forbidden {
      assert!(
        !condensed.contains(needle),
        "{} must not contain `{}`; configure the renderer with FontConfig::bundled_only() instead",
        path.display(),
        needle
      );
    }
  }
}

