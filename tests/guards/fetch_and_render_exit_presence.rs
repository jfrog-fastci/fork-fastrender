//! Guard against accidental deletion of the fetch_and_render exit regression.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

#[test]
fn fetch_and_render_exit_regression_is_present() {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let marker = "fetch_and_render_exits_non_zero_when_no_args";

  let found = find_marker_in_rust_sources(&root, marker);
  assert!(
    found.is_some(),
    "fetch_and_render exit regression test coverage must exist (searched src/**/*.rs and tests/**/*.rs for {marker:?})"
  );
}

fn find_marker_in_rust_sources(root: &Path, marker: &str) -> Option<PathBuf> {
  let self_path = root.join(file!());
  for dir in ["src", "tests"] {
    let dir = root.join(dir);
    if !dir.exists() {
      continue;
    }
    let is_tests_dir = dir.file_name() == Some(OsStr::new("tests"));

    for entry in WalkDir::new(&dir)
      .into_iter()
      .filter_entry(|entry| !is_tests_dir || !super::should_skip_tests_entry(entry, &dir))
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
      if source.contains(marker) {
        return Some(path.strip_prefix(root).unwrap_or(path).to_path_buf());
      }
    }
  }

  None
}
