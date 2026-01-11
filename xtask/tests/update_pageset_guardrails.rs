use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProgressStatus {
  Ok,
  Timeout,
  Panic,
  Error,
}

impl ProgressStatus {
  fn is_failure(self) -> bool {
    matches!(self, ProgressStatus::Timeout | ProgressStatus::Panic | ProgressStatus::Error)
  }
}

#[derive(Debug, Deserialize)]
struct ProgressPage {
  status: ProgressStatus,
  #[serde(default)]
  auto_notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GuardrailsManifest {
  fixtures: Vec<GuardrailsFixture>,
}

#[derive(Debug, Deserialize)]
struct GuardrailsFixture {
  name: String,
}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has parent")
    .to_path_buf()
}

fn is_sync_placeholder(status: ProgressStatus, auto_notes: Option<&str>) -> bool {
  if status != ProgressStatus::Error {
    return false;
  }
  match auto_notes.map(|s| s.trim()) {
    Some("not run") | Some("missing cache") => true,
    _ => false,
  }
}

fn read_fixture_names(manifest_path: &Path) -> BTreeSet<String> {
  let manifest_raw = fs::read_to_string(manifest_path).expect("read pageset guardrails manifest");
  let parsed: GuardrailsManifest =
    serde_json::from_str(&manifest_raw).expect("parse pageset guardrails manifest");
  parsed.fixtures.into_iter().map(|f| f.name).collect()
}

#[test]
fn guardrails_manifest_excludes_sync_placeholders_but_keeps_real_failures() {
  let root = repo_root();
  let progress_dir = root.join("progress/pages");
  let manifest_path = root.join("tests/pages/pageset_guardrails.json");

  let fixture_names = read_fixture_names(&manifest_path);
  assert!(
    !fixture_names.is_empty(),
    "{} should contain at least one fixture",
    manifest_path.display()
  );

  let mut placeholders = BTreeSet::new();
  let mut real_failures = BTreeSet::new();
  let mut ok_count = 0usize;

  let mut progress_paths: Vec<PathBuf> = fs::read_dir(&progress_dir)
    .expect("read progress/pages dir")
    .filter_map(|entry| entry.ok().map(|e| e.path()))
    .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("json"))
    .collect();
  progress_paths.sort();

  for path in progress_paths {
    let name = path
      .file_stem()
      .and_then(|s| s.to_str())
      .expect("progress/pages filename is utf-8");
    let raw = fs::read_to_string(&path).expect("read progress entry");
    let parsed: ProgressPage = serde_json::from_str(&raw).expect("parse progress entry");
    if parsed.status == ProgressStatus::Ok {
      ok_count += 1;
    }
    let auto_notes = parsed.auto_notes.as_deref();
    if is_sync_placeholder(parsed.status, auto_notes) {
      placeholders.insert(name.to_string());
    } else if parsed.status.is_failure() {
      real_failures.insert(name.to_string());
    }
  }

  let required_ok_pages = fixture_names.len().saturating_sub(real_failures.len());
  assert!(
    ok_count >= required_ok_pages,
    "expected progress/pages to contain at least {required_ok_pages} ok page(s), got {ok_count}"
  );

  let placeholders_in_manifest: Vec<String> = placeholders
    .intersection(&fixture_names)
    .cloned()
    .collect();
  assert!(
    placeholders_in_manifest.is_empty(),
    "{} should not include pageset_progress sync placeholders: {}",
    manifest_path.display(),
    placeholders_in_manifest.join(", ")
  );

  let missing_failures: Vec<String> = real_failures
    .difference(&fixture_names)
    .cloned()
    .collect();
  assert!(
    missing_failures.is_empty(),
    "{} is missing real failing pages from progress/pages: {}",
    manifest_path.display(),
    missing_failures.join(", ")
  );
}
