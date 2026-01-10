use fastrender::pageset::pageset_entries;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[test]
fn pageset_progress_json_matches_pageset_entries() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
  let progress_dir = repo_root.join("progress/pages");

  let entries = pageset_entries();
  let expected_len = entries.len();
  let expected: BTreeSet<String> = entries.into_iter().map(|entry| entry.cache_stem).collect();
  assert_eq!(
    expected.len(),
    expected_len,
    "pageset cache stems must be unique"
  );

  let mut actual: BTreeSet<String> = BTreeSet::new();
  for entry in fs::read_dir(&progress_dir).expect("read progress/pages directory") {
    let entry = entry.expect("read progress/pages entry");
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
      continue;
    }
    let stem = path
      .file_stem()
      .unwrap_or_default()
      .to_string_lossy()
      .into_owned();
    if !stem.is_empty() {
      actual.insert(stem);
    }
  }

  let missing: Vec<String> = expected.difference(&actual).cloned().collect();
  let extra: Vec<String> = actual.difference(&expected).cloned().collect();

  assert!(
    missing.is_empty() && extra.is_empty(),
    "progress/pages is out of sync with src/pageset.rs\nmissing: {}\nextra: {}",
    if missing.is_empty() {
      "(none)".to_string()
    } else {
      missing.join(", ")
    },
    if extra.is_empty() {
      "(none)".to_string()
    } else {
      extra.join(", ")
    }
  );
}

