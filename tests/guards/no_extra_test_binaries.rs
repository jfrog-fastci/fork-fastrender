//! Guard that enforces the post-cleanup integration-test crate layout.
//!
//! Cargo treats each top-level `tests/*.rs` file as its own integration-test binary. After
//! consolidating the suite into `tests/integration.rs`, we want to keep the number of binaries
//! stable (linking dominates build time for this crate).
//!
//! Special-case integration test binaries should be extremely rare (e.g. custom `#[global_allocator]`
//! harnesses like `tests/allocation_failure.rs`).

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn list_top_level_test_crates(root: &PathBuf) -> BTreeSet<String> {
  let tests_dir = root.join("tests");
  let mut crates = BTreeSet::new();

  let entries = fs::read_dir(&tests_dir)
    .unwrap_or_else(|err| panic!("failed to read tests dir {}: {err}", tests_dir.display()));
  for entry in entries {
    let entry = entry.expect("read tests dir entry");
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    let rel = path
      .strip_prefix(root)
      .unwrap_or(&path)
      .display()
      .to_string();
    crates.insert(rel);
  }

  crates
}

#[test]
fn no_extra_integration_test_binaries_exist() {
  let root = repo_root();
  let actual = list_top_level_test_crates(&root);
  let expected = BTreeSet::from([
    "tests/allocation_failure.rs".to_string(),
    "tests/integration.rs".to_string(),
  ]);

  assert!(
    actual == expected,
    "unexpected set of top-level integration test crates (tests/*.rs).\n\
     Expected: {expected:?}\n\
     Actual:   {actual:?}\n\
     \n\
     Do not add new `tests/*.rs` files: each one creates a new integration test binary.\n\
     Add new suites/modules under `tests/integration.rs` instead.\n\
     If you truly need a special-harness binary, update this guard and document it in \
     progress/test_cleanup_inventory.md."
  );
}

