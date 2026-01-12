//! Guards against accidental deletion of critical style regression tests.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

// Use a marker that is stable across file moves/renames: it should match the test function names
// regardless of whether the regression lives under `tests/` or is migrated into `src/`.
const NEEDLE: &str = "fn background_position_logical_";

fn find_marker_in_src_style(root: &Path, needle: &str) -> Option<PathBuf> {
  let dir = root.join("src").join("style");
  if !dir.exists() {
    return None;
  }

  for entry in WalkDir::new(&dir)
    .into_iter()
    .filter_entry(|entry| !super::should_skip_tests_entry(entry, &dir))
    .filter_map(std::result::Result::ok)
    .filter(|entry| entry.file_type().is_file())
  {
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }

    let Ok(source) = fs::read_to_string(path) else {
      continue;
    };
    if source.contains(needle) {
      return Some(path.strip_prefix(root).unwrap_or(path).to_path_buf());
    }
  }

  None
}

fn find_marker_in_tests(root: &Path, needle: &str) -> Option<PathBuf> {
  let dir = root.join("tests");
  if !dir.exists() {
    return None;
  }

  for entry in WalkDir::new(&dir)
    .into_iter()
    .filter_map(std::result::Result::ok)
    .filter(|entry| entry.file_type().is_file())
  {
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if !file_name.contains("background_position_logical") {
      continue;
    }

    let Ok(source) = fs::read_to_string(path) else {
      continue;
    };
    if source.contains(needle) {
      return Some(path.strip_prefix(root).unwrap_or(path).to_path_buf());
    }
  }

  None
}

#[test]
fn background_position_logical_regression_is_present() {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

  if let Some(path) = find_marker_in_src_style(&root, NEEDLE) {
    eprintln!("background_position_logical regression found in {path:?}");
    return;
  }

  if let Some(path) = find_marker_in_tests(&root, NEEDLE) {
    eprintln!("background_position_logical regression found in {path:?}");
    return;
  }

  panic!(
    "background_position_logical regression test coverage must exist (searched src/style/**/*.rs and tests/**/*.rs for {NEEDLE:?})"
  );
}
