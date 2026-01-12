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

fn find_background_position_logical_test_in_tests(root: &Path) -> Option<PathBuf> {
  let tests_dir = root.join("tests");
  if !tests_dir.exists() {
    return None;
  }

  fn find_in_dir(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).expect("read_dir tests/") {
      let entry = entry.expect("read_dir entry");
      let path = entry.path();
      if path.is_dir() {
        if let Some(found) = find_in_dir(&path) {
          return Some(found);
        }
        continue;
      }
      if !path.extension().is_some_and(|ext| ext == "rs") {
        continue;
      }
      let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        continue;
      };
      if !file_name.contains("background_position_logical") {
        continue;
      }
      let contents = fs::read_to_string(&path).expect("read candidate regression test source");
      if contents.contains(NEEDLE) {
        return Some(path);
      }
    }
    None
  }

  find_in_dir(&tests_dir)
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

  // Transitional location: an integration test module under `tests/`.
  if let Some(path) = find_background_position_logical_test_in_tests(root) {
    let contents = fs::read_to_string(&path).expect("read integration test source");
    assert!(
      contents.contains(NEEDLE),
      "expected {:?} to contain the background_position_logical regression test ({NEEDLE})",
      path
    );
    return;
  }

  panic!(
    "background_position_logical regression must be present either as a unit test under src/style/ \
     (containing {NEEDLE}) or as an integration test module under tests/"
  );
}
