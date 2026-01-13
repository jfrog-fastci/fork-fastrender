//! Guard against committing unresolved merge-conflict markers.

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

const MERGE_MARKERS: [&str; 4] = [
  concat!("<<<", "<<", "<<"),
  concat!("|||", "|||", "|"),
  concat!("===", "==", "=="),
  concat!(">>>", ">>>", ">"),
];

#[test]
fn no_merge_conflict_markers_present() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let search_roots = [
    repo_root.join("src"),
    repo_root.join("tests"),
    repo_root.join("benches"),
    repo_root.join("fuzz"),
  ];

  let mut offenders = Vec::new();

  for root in search_roots {
    if !root.exists() {
      continue;
    }

    for path in rust_files(&root) {
      let rel_path = path
        .strip_prefix(&repo_root)
        .map(|p| p.to_path_buf())
        .unwrap_or(path.clone());

      let file = fs::File::open(&path).unwrap_or_else(|err| {
        panic!("failed to open {}: {}", rel_path.display(), err);
      });

      for (line_idx, line) in BufReader::new(file).lines().enumerate() {
        let line_number = line_idx + 1;
        let line = line.unwrap_or_else(|err| {
          panic!("failed to read {}: {}", rel_path.display(), err);
        });

        for marker in MERGE_MARKERS {
          if line.trim_start().starts_with(marker) {
            offenders.push(format!(
              "{}:{line_number}: contains merge-conflict marker {marker}",
              rel_path.display()
            ));
          }
        }
      }
    }
  }

  if !offenders.is_empty() {
    panic!(
      "found merge-conflict markers in Rust sources:\n{}",
      offenders.join("\n")
    );
  }
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
  let mut files = Vec::new();
  let is_tests_tree = root.file_name() == Some(OsStr::new("tests"));
  for entry in WalkDir::new(root)
    .into_iter()
    .filter_entry(|entry| !is_tests_tree || !super::should_skip_tests_entry(entry, root))
    .filter_map(Result::ok)
  {
    let path = entry.path();
    if entry.file_type().is_file() && path.extension() == Some(OsStr::new("rs")) {
      files.push(path.to_path_buf());
    }
  }

  files.sort();
  files
}
