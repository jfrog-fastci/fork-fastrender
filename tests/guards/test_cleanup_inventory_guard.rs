//! Guard that enforces `progress/test_cleanup_inventory.md` stays in sync with `tests/*.rs`.
//!
//! The test cleanup project is aggressively consolidating/removing integration test crates. During
//! that churn it is easy for parallel PRs to add/remove a `tests/*.rs` crate and forget to update
//! the inventory table (or vice-versa). This guard fails fast when the inventory is stale.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn list_top_level_test_crates(root: &Path) -> BTreeSet<String> {
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
    let file_name = path
      .file_name()
      .and_then(|name| name.to_str())
      .expect("tests/*.rs file name must be valid UTF-8");
    crates.insert(format!("tests/{file_name}"));
  }
  crates
}

fn parse_active_inventory_section(inventory: &str) -> BTreeSet<String> {
  const ACTIVE_HEADING: &str = "### Active top-level crates (HEAD)";

  let mut in_active = false;
  let mut crates = BTreeSet::new();
  for line in inventory.lines() {
    let line = line.trim_end();
    if line.trim() == ACTIVE_HEADING {
      in_active = true;
      continue;
    }
    if !in_active {
      continue;
    }
    if line.trim_start().starts_with("### ") {
      break;
    }
    if !line.starts_with('|') {
      continue;
    }

    // Table rows look like:
    // | `tests/foo.rs` | unit | ... |
    let Some(first_tick) = line.find('`') else {
      continue;
    };
    let rest = &line[first_tick + 1..];
    let Some(second_tick) = rest.find('`') else {
      continue;
    };
    let path = &rest[..second_tick];
    if path.starts_with("tests/") && path.ends_with(".rs") {
      crates.insert(path.to_string());
    }
  }

  assert!(
    in_active,
    "inventory is missing the heading {ACTIVE_HEADING:?}; cannot validate active test crates"
  );
  crates
}

#[test]
fn test_cleanup_inventory_tracks_all_top_level_test_crates() {
  let root = repo_root();
  let inventory_path = root.join("progress").join("test_cleanup_inventory.md");
  let inventory = fs::read_to_string(&inventory_path).unwrap_or_else(|err| {
    panic!(
      "failed to read inventory file {}: {err}",
      inventory_path.display()
    )
  });

  let actual = list_top_level_test_crates(&root);
  let active = parse_active_inventory_section(&inventory);

  let missing: Vec<_> = actual.difference(&active).cloned().collect();
  let extra: Vec<_> = active.difference(&actual).cloned().collect();

  assert!(
    missing.is_empty() && extra.is_empty(),
    "test cleanup inventory is out of date:\n\
     - missing rows for: {missing:?}\n\
     - has rows for non-existent crates: {extra:?}\n\
     Update the \"Active top-level crates (HEAD)\" table in {} to match `ls tests/*.rs`.",
    inventory_path.display()
  );
}

