//! Guards against accidental deletion of critical style regression tests.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

// Use a marker that is stable across file moves/renames: it should match the test function names
// regardless of whether the regression lives under `tests/` or is migrated into `src/`.
const NEEDLE: &str = "fn background_position_logical_";

#[test]
fn background_position_logical_regression_is_present() {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

  let found = find_marker_in_rust_sources(&root, NEEDLE);
  assert!(
    found.is_some(),
    "background-position logical regression test coverage must exist (searched src/**/*.rs and tests/**/*.rs for {NEEDLE:?})"
  );
}

fn find_marker_in_rust_sources(root: &Path, needle: &str) -> Option<PathBuf> {
  let self_path = root.join(file!());
  for dir in ["src", "tests"] {
    let dir = root.join(dir);
    if !dir.exists() {
      continue;
    }

    for entry in WalkDir::new(&dir)
      .into_iter()
      .filter_map(std::result::Result::ok)
      .filter(|entry| entry.file_type().is_file())
    {
      let path = entry.path();
      if path == self_path {
        continue;
      }
      if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        continue;
      }

      let Ok(source) = std::fs::read_to_string(path) else {
        continue;
      };
      if source.contains(needle) {
        return Some(path.strip_prefix(root).unwrap_or(path).to_path_buf());
      }
    }
  }

  None
}
