//! Guards against accidental deletion of critical style regression tests.

use std::fs;
use std::path::{Path, PathBuf};

const NEEDLE: &str = "fn background_position_logical_";

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let path = entry.path();
    if path.is_dir() {
      collect_rs_files(&path, out)?;
      continue;
    }
    if path.extension().is_some_and(|ext| ext == "rs") {
      out.push(path);
    }
  }
  Ok(())
}

fn find_background_position_logical_test_in_src(root: &Path) -> Option<PathBuf> {
  let src_style_dir = root.join("src").join("style");
  if !src_style_dir.exists() {
    return None;
  }

  let mut rs_files = Vec::new();
  collect_rs_files(&src_style_dir, &mut rs_files).expect("walk src/style");

  for path in rs_files {
    let contents = fs::read_to_string(&path).expect("read src/style file");
    if contents.contains(NEEDLE) {
      return Some(path);
    }
  }
  None
}

#[test]
fn background_position_logical_regression_is_present() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"));

  // After test cleanup, this regression should live as a unit test under `src/style`.
  if let Some(path) = find_background_position_logical_test_in_src(root) {
    let contents = fs::read_to_string(&path).expect("read unit test source");
    assert!(
      contents.contains(NEEDLE),
      "expected {:?} to contain the background_position_logical regression test ({NEEDLE})",
      path
    );
    return;
  }

  // Legacy location (pre-cleanup).
  let legacy_path = root
    .join("tests")
    .join("style")
    .join("background_position_logical_test.rs");
  assert!(
    legacy_path.exists(),
    "background_position_logical regression must be present either as a unit test under src/style/ \
     (containing {NEEDLE}) or in the legacy integration test file at {:?}",
    legacy_path
  );
  let contents = fs::read_to_string(&legacy_path).expect("read legacy background_position_logical test");
  assert!(
    contents.contains(NEEDLE),
    "expected {:?} to contain the background_position_logical regression test ({NEEDLE})",
    legacy_path
  );
}
