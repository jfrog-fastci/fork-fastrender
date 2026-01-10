use fastrender::pageset::pageset_entries;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn repo_root() -> std::path::PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn pageset_progress_json_matches_pageset_entries() {
  let repo_root = repo_root();
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

#[test]
fn pageset_fixtures_exist_for_all_entries() {
  let repo_root = repo_root();
  let fixtures_root = repo_root.join("tests/pages/fixtures");

  let expected: BTreeSet<String> = pageset_entries()
    .into_iter()
    .map(|entry| entry.cache_stem)
    .collect();

  let mut missing = Vec::new();
  for stem in expected {
    let path = fixtures_root.join(&stem).join("index.html");
    if !path.is_file() {
      missing.push(format!("{stem} ({})", path.display()));
    }
  }

  assert!(
    missing.is_empty(),
    "tests/pages/fixtures is missing pageset fixture(s): {}",
    if missing.is_empty() {
      "(none)".to_string()
    } else {
      missing.join(", ")
    }
  );
}

#[test]
fn pageset_progress_entries_include_accuracy() {
  let repo_root = repo_root();
  let progress_dir = repo_root.join("progress/pages");

  let expected: BTreeSet<String> = pageset_entries()
    .into_iter()
    .map(|entry| entry.cache_stem)
    .collect();

  let mut missing = Vec::new();
  for stem in expected {
    let path = progress_dir.join(format!("{stem}.json"));
    let raw = fs::read_to_string(&path)
      .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let value: serde_json::Value =
      serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));
    if value.get("accuracy").is_none() {
      missing.push(stem);
    }
  }

  assert!(
    missing.is_empty(),
    "progress/pages entries missing accuracy: {}",
    if missing.is_empty() {
      "(none)".to_string()
    } else {
      missing.join(", ")
    }
  );
}
