//! Guard that rejects `#[path = <path>]` shims anywhere under `tests/`.
//!
//! The test architecture cleanup project consolidates integration tests into a small number of
//! binaries. `#[path]` shims in `tests/` are a common footgun: they usually exist only to create a
//! dedicated `cargo test --test ...` entry-point, but they silently create additional integration
//! test crates and undo consolidation work.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use walkdir::WalkDir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn no_path_shims_in_tests_tree() {
  let root = repo_root();
  let tests_dir = root.join("tests");

  let mut files = Vec::new();
  for entry in WalkDir::new(&tests_dir)
    .into_iter()
    .filter_entry(|entry| !super::should_skip_tests_entry(entry, &tests_dir))
  {
    let entry =
      entry.unwrap_or_else(|err| panic!("walk tests dir while scanning for #[path] shims: {err}"));
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension() != Some(OsStr::new("rs")) {
      continue;
    }
    files.push(path.to_path_buf());
  }
  files.sort();

  let mut offenders = Vec::new();
  for path in files {
    let content = fs::read_to_string(&path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    for (idx, line) in content.lines().enumerate() {
      let trimmed = line.trim_start();
      if trimmed.starts_with("#[path") {
        offenders.push(format!(
          "{}:{}: {}",
          path.strip_prefix(&root).unwrap_or(&path).display(),
          idx + 1,
          trimmed
        ));
      }
    }
  }

  assert!(
    offenders.is_empty(),
    "found #[path] shims under tests/ (these create extra integration test crates):\n{}",
    offenders.join("\n")
  );
}
