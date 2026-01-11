use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[derive(Deserialize)]
struct GuardrailsManifest {
  fixtures: Vec<GuardrailsFixture>,
}

#[derive(Deserialize)]
struct GuardrailsFixture {
  name: String,
  url: Option<String>,
}

#[derive(Deserialize)]
struct ProgressEntry {
  url: String,
}

#[test]
fn pageset_timeouts_manifest_is_legacy_guardrails_mirror() {
  let root = repo_root();
  let guardrails_path = root.join("tests/pages/pageset_guardrails.json");
  let legacy_path = root.join("tests/pages/pageset_timeouts.json");

  let guardrails_raw =
    fs::read_to_string(&guardrails_path).expect("read pageset_guardrails manifest");
  let legacy_raw = fs::read_to_string(&legacy_path).expect("read pageset_timeouts manifest");

  let guardrails: serde_json::Value =
    serde_json::from_str(&guardrails_raw).expect("parse pageset_guardrails manifest");
  let legacy: serde_json::Value =
    serde_json::from_str(&legacy_raw).expect("parse pageset_timeouts manifest");

  assert_eq!(
    guardrails,
    legacy,
    "legacy manifest {} should mirror {}",
    legacy_path.display(),
    guardrails_path.display()
  );
}

#[test]
fn pageset_guardrails_manifest_urls_match_progress() {
  let root = repo_root();
  let progress_dir = root.join("progress/pages");
  let manifest_path = root.join("tests/pages/pageset_guardrails.json");

  let manifest_raw = fs::read_to_string(&manifest_path).expect("read pageset guardrails manifest");
  let manifest: GuardrailsManifest =
    serde_json::from_str(&manifest_raw).expect("parse pageset guardrails manifest");

  let mut progress_urls = BTreeMap::new();
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
    if stem.is_empty() {
      continue;
    }
    let raw = fs::read_to_string(&path).expect("read progress entry json");
    let entry: ProgressEntry = serde_json::from_str(&raw).expect("parse progress entry json");
    progress_urls.insert(stem, entry.url);
  }

  let mut mismatches = Vec::new();
  for fixture in &manifest.fixtures {
    let expected = match progress_urls.get(&fixture.name) {
      Some(url) => url,
      None => {
        mismatches.push(format!(
          "{} missing progress/pages/{}.json",
          fixture.name, fixture.name
        ));
        continue;
      }
    };

    match fixture.url.as_deref() {
      Some(actual) if actual == expected => {}
      Some(actual) => mismatches.push(format!(
        "{} url mismatch: manifest={} progress={}",
        fixture.name, actual, expected
      )),
      None => mismatches.push(format!(
        "{} missing url field (expected {})",
        fixture.name, expected
      )),
    }
  }

  assert!(
    mismatches.is_empty(),
    "{} fixture URLs drifted from progress/pages:\n{}",
    manifest_path.display(),
    mismatches.join("\n")
  );
}
