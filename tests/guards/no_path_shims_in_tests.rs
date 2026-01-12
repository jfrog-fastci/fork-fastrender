//! Guard that rejects `#[path = <path>]` shims anywhere under `tests/`.
//!
//! The test architecture cleanup project consolidates integration tests into a small number of
//! binaries. `#[path]` shims in `tests/` are a common footgun: they usually exist only to create a
//! dedicated `cargo test --test ...` entry-point, but they silently create additional integration
//! test crates and undo consolidation work.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
  let entries = fs::read_dir(dir).unwrap_or_else(|err| {
    panic!(
      "failed to read dir {} while scanning for #[path] shims: {err}",
      dir.display()
    )
  });
  for entry in entries {
    let entry = entry.expect("read dir entry");
    let path = entry.path();
    if path.is_dir() {
      collect_rs_files(&path, out);
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    out.push(path);
  }
}

#[test]
fn no_path_shims_in_tests_tree() {
  let root = repo_root();
  let tests_dir = root.join("tests");

  let mut files = Vec::new();
  collect_rs_files(&tests_dir, &mut files);
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
