//! Static integrity validator for `tests/wpt/manifest.toml`.
//!
//! Rendering-based WPT tests are expensive and can hide simple corpus integrity issues
//! (missing files, broken references, missing expected PNGs, etc.). This module provides a
//! fast, non-rendering validator that fails with actionable messages.

use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
struct ManifestFile {
  tests: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ManifestEntry {
  path: String,
  #[serde(default)]
  reference: Option<String>,
  #[serde(default)]
  test_type: Option<String>,
  #[serde(default)]
  id: Option<String>,
  #[serde(default)]
  viewport: Option<ManifestViewport>,
  #[serde(default)]
  timeout_ms: Option<u64>,
  #[serde(default)]
  dpr: Option<f32>,
  #[serde(default)]
  expected: Option<String>,
  #[serde(default)]
  media: Option<String>,
  #[serde(default)]
  fit_canvas_to_content: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ManifestViewport {
  width: u32,
  height: u32,
}

fn load_manifest(manifest_path: &Path) -> Result<ManifestFile, String> {
  let raw = fs::read_to_string(manifest_path)
    .map_err(|e| format!("Failed to read manifest at {manifest_path:?}: {e}"))?;
  toml::from_str(&raw).map_err(|e| format!("Failed to parse TOML manifest at {manifest_path:?}: {e}"))
}

fn normalize_id_from_rel_path(rel: &Path) -> String {
  let mut stem = rel.to_path_buf();
  stem.set_extension("");
  stem.to_string_lossy().replace('\\', "/")
}

fn parse_rel_path(field_name: &str, raw: &str, entry_id: &str) -> Result<PathBuf, String> {
  if raw.trim().is_empty() {
    return Err(format!("Test {entry_id}: `{field_name}` must not be empty"));
  }

  let path = PathBuf::from(raw);
  if path.is_absolute() {
    return Err(format!(
      "Test {entry_id}: `{field_name}` must be a relative path (got {raw:?})"
    ));
  }

  for component in path.components() {
    match component {
      Component::ParentDir => {
        return Err(format!(
          "Test {entry_id}: `{field_name}` must not contain `..` path traversal (got {raw:?})"
        ));
      }
      Component::CurDir => {
        return Err(format!(
          "Test {entry_id}: `{field_name}` must be normalized (no `.` segments; got {raw:?})"
        ));
      }
      Component::Prefix(_) | Component::RootDir => {
        return Err(format!(
          "Test {entry_id}: `{field_name}` must not be an absolute/path-prefixed value (got {raw:?})"
        ));
      }
      Component::Normal(_) => {}
    }
  }

  Ok(path)
}

fn expected_png_path(expected_root: &Path, tests_root: &Path, test_path: &Path) -> PathBuf {
  // Keep this logic in sync with `WptRunner::get_expected_image_path`.
  let relative = test_path.strip_prefix(tests_root).unwrap_or(test_path);
  let mut expected_path = expected_root.join(relative);
  expected_path.set_extension("png");
  expected_path
}

fn validate_manifest(manifest_path: &Path, tests_root: &Path, expected_root: &Path) -> Result<(), String> {
  let manifest = load_manifest(manifest_path)?;

  let mut errors = Vec::new();
  let mut seen_ids = HashSet::new();
  let mut seen_paths = HashSet::new();

  for entry in &manifest.tests {
    let entry_id = entry
      .id
      .as_deref()
      .unwrap_or("<missing id>");

    let rel_path = match parse_rel_path("path", &entry.path, entry_id) {
      Ok(path) => path,
      Err(e) => {
        errors.push(e);
        continue;
      }
    };

    let derived_id = normalize_id_from_rel_path(&rel_path);
    if let Some(id) = &entry.id {
      if id != &derived_id {
        errors.push(format!(
          "Test {entry_id}: `id` must match the normalized path stem. Expected {derived_id:?}, got {id:?}"
        ));
      }
    }

    let effective_id = entry.id.clone().unwrap_or(derived_id);
    if !seen_ids.insert(effective_id.clone()) {
      errors.push(format!("Duplicate test id {effective_id:?} in manifest"));
    }

    if !seen_paths.insert(rel_path.clone()) {
      errors.push(format!(
        "Duplicate test path {:?} in manifest (id {effective_id:?})",
        entry.path
      ));
    }

    let test_file = tests_root.join(&rel_path);
    if !test_file.is_file() {
      errors.push(format!(
        "Test {effective_id}: `path` points to missing file {test_file:?}"
      ));
    }

    if let Some(reference) = &entry.reference {
      match parse_rel_path("reference", reference, &effective_id) {
        Ok(reference_rel) => {
          let reference_file = tests_root.join(reference_rel);
          if !reference_file.is_file() {
            errors.push(format!(
              "Test {effective_id}: `reference` points to missing file {reference_file:?}"
            ));
          }
        }
        Err(e) => errors.push(e),
      }
    }

    if entry
      .test_type
      .as_deref()
      .map(|t| t.eq_ignore_ascii_case("visual"))
      .unwrap_or(false)
    {
      let expected_png = expected_png_path(expected_root, tests_root, &test_file);
      if !expected_png.is_file() {
        errors.push(format!(
          "Test {effective_id}: visual test is missing expected PNG {expected_png:?}"
        ));
      }
    }
  }

  if errors.is_empty() {
    Ok(())
  } else {
    Err(format!(
      "WPT manifest validation failed with {} error(s):\n- {}",
      errors.len(),
      errors.join("\n- ")
    ))
  }
}

#[test]
fn wpt_manifest_is_valid() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_path = repo_root.join("tests/wpt/manifest.toml");
  let tests_root = repo_root.join("tests/wpt/tests");
  let expected_root = repo_root.join("tests/wpt/expected");

  validate_manifest(&manifest_path, &tests_root, &expected_root).unwrap();
}

#[test]
fn wpt_manifest_validator_reports_missing_expected_png() {
  let tmp = tempfile::TempDir::new().unwrap();
  let tests_root = tmp.path().join("tests");
  let expected_root = tmp.path().join("expected");
  fs::create_dir_all(&tests_root).unwrap();
  fs::create_dir_all(&expected_root).unwrap();

  fs::write(tests_root.join("missing.png.html"), "<!doctype html><p>hi</p>").unwrap();

  let manifest_path = tmp.path().join("manifest.toml");
  fs::write(
    &manifest_path,
    r#"
[[tests]]
id = "missing.png"
path = "missing.png.html"
test_type = "visual"
"#,
  )
  .unwrap();

  let err = validate_manifest(&manifest_path, &tests_root, &expected_root).unwrap_err();
  assert!(
    err.contains("missing expected PNG"),
    "error should mention missing expected PNG, got:\n{err}"
  );
  assert!(
    err.contains("missing.png"),
    "error should mention test id/path, got:\n{err}"
  );
  assert!(
    err.contains("missing.png.png"),
    "error should include derived expected PNG filename, got:\n{err}"
  );
}
