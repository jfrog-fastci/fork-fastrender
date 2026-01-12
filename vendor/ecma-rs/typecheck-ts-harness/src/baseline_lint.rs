use crate::diagnostic_norm::sort_diagnostics;
use crate::strict_native::{StrictNativeBaseline, STRICT_NATIVE_BASELINE_SCHEMA_VERSION};
use crate::tsc::{TscDiagnostics, TSC_BASELINE_SCHEMA_VERSION};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Lint committed JSON baseline artifacts under `typecheck-ts-harness/baselines/**`.
///
/// This is intentionally lightweight (JSON parse + a few ordering/version checks)
/// so it can run in CI without requiring Node.js or executing tsc.
pub fn lint_baselines() -> Result<()> {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  let crate_label = manifest_dir
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("typecheck-ts-harness");
  let baselines_root = manifest_dir.join("baselines");
  if !baselines_root.is_dir() {
    anyhow::bail!(
      "lint-baselines: baselines directory not found at {}",
      baselines_root.display()
    );
  }

  let pinned_typescript_version = pinned_typescript_version(manifest_dir)
    .context("lint-baselines: read pinned TypeScript version from package-lock.json")?;

  let mut files: Vec<(String, PathBuf)> = WalkDir::new(&baselines_root)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
    .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
    .map(|entry| {
      let path = entry.into_path();
      let rel = match path.strip_prefix(manifest_dir) {
        Ok(rel) => format!("{crate_label}/{}", rel.to_string_lossy()),
        Err(_) => path.to_string_lossy().to_string(),
      }
      .replace('\\', "/");
      (rel, path)
    })
    .collect();
  files.sort_by(|(a, _), (b, _)| a.cmp(b));

  let mut errors: Vec<String> = Vec::new();
  for (rel, path) in files {
    let raw = std::fs::read_to_string(&path).with_context(|| format!("{rel}: read baseline"))?;
    let file_errors = lint_baseline_file(&baselines_root, &path, &raw, &pinned_typescript_version);
    for err in file_errors {
      errors.push(format!("{rel}: {err}"));
    }
  }

  if errors.is_empty() {
    return Ok(());
  }

  anyhow::bail!(
    "lint-baselines: {} error(s) found:\n{}",
    errors.len(),
    errors.join("\n")
  );
}

fn pinned_typescript_version(manifest_dir: &Path) -> Result<String> {
  let lock_path = manifest_dir.join("package-lock.json");
  let raw =
    std::fs::read_to_string(&lock_path).with_context(|| format!("read {}", lock_path.display()))?;
  let lock: Value =
    serde_json::from_str(&raw).with_context(|| format!("parse {}", lock_path.display()))?;

  // npm lockfile v3 (npm 7+) layout:
  //
  // "packages": {
  //   "": { "dependencies": { "typescript": "5.x.y" } },
  //   "node_modules/typescript": { "version": "5.x.y", ... }
  // }
  //
  // Use the pinned dependency string from the root package entry when present;
  // it matches `typecheck-ts-harness/package.json`.
  if let Some(version) = lock
    .pointer("/packages//dependencies/typescript")
    .and_then(|v| v.as_str())
  {
    return Ok(version.to_string());
  }

  // Fallback for older lockfile layouts where the installed package version is
  // recorded under `dependencies.typescript.version`.
  if let Some(version) = lock
    .pointer("/dependencies/typescript/version")
    .and_then(|v| v.as_str())
  {
    return Ok(version.to_string());
  }

  // Last resort: use the resolved `node_modules/typescript` version if present.
  if let Some(version) = lock
    .pointer("/packages/node_modules~1typescript/version")
    .and_then(|v| v.as_str())
  {
    return Ok(version.to_string());
  }

  anyhow::bail!(
    "failed to locate TypeScript version in {}; expected npm lockfile v3 (packages[\"\"] deps) or v1/v2 (dependencies.typescript.version)",
    lock_path.display()
  )
}

fn lint_baseline_file(
  baselines_root: &Path,
  path: &Path,
  raw: &str,
  pinned_typescript_version: &str,
) -> Vec<String> {
  let rel = path
    .strip_prefix(baselines_root)
    .unwrap_or(path)
    .to_string_lossy()
    .replace('\\', "/");
  let suite = rel.split('/').next().unwrap_or("");
  match suite {
    "strict-native" => lint_strict_native_baseline(raw),
    // Everything else under `baselines/**` currently uses the `TscDiagnostics`
    // snapshot schema (difftsc + conformance snapshots).
    _ => lint_tsc_diagnostics_baseline(raw, pinned_typescript_version),
  }
}

fn lint_tsc_diagnostics_baseline(raw: &str, pinned_typescript_version: &str) -> Vec<String> {
  let baseline: TscDiagnostics = match serde_json::from_str(raw) {
    Ok(baseline) => baseline,
    Err(err) => {
      return vec![format!("failed to parse as tsc baseline schema: {err}")];
    }
  };

  let mut errors = Vec::new();

  if baseline.schema_version != Some(TSC_BASELINE_SCHEMA_VERSION) {
    errors.push(format!(
      "schema_version mismatch (expected {TSC_BASELINE_SCHEMA_VERSION}, got {:?})",
      baseline.schema_version
    ));
  }

  match baseline.metadata.typescript_version.as_deref() {
    Some(v) if !v.trim().is_empty() => {
      let v = v.trim();
      if v != pinned_typescript_version {
        errors.push(format!(
          "metadata.typescript_version mismatch (expected {pinned_typescript_version}, got {v})"
        ));
      }
    }
    _ => errors.push("metadata.typescript_version missing or empty".to_string()),
  }

  if !is_tsc_diagnostics_sorted(&baseline.diagnostics) {
    errors.push("diagnostics are not in canonical sorted order".to_string());
  }

  if let Some(type_facts) = baseline.type_facts.as_ref() {
    if !is_export_type_facts_sorted(&type_facts.exports) {
      errors.push("type_facts.exports are not in canonical sorted order".to_string());
    }
    if !is_marker_type_facts_sorted(&type_facts.markers) {
      errors.push("type_facts.markers are not in canonical sorted order".to_string());
    }
  }

  errors
}

fn is_tsc_diagnostics_sorted(diags: &[crate::tsc::TscDiagnostic]) -> bool {
  diags.windows(2).all(|pair| {
    let a = &pair[0];
    let b = &pair[1];
    (a.file.as_deref().unwrap_or(""), a.start, a.end, a.code)
      <= (b.file.as_deref().unwrap_or(""), b.start, b.end, b.code)
  })
}

fn is_export_type_facts_sorted(diags: &[crate::tsc::ExportTypeFact]) -> bool {
  diags.windows(2).all(|pair| {
    (&pair[0].file, &pair[0].name, &pair[0].type_str)
      <= (&pair[1].file, &pair[1].name, &pair[1].type_str)
  })
}

fn is_marker_type_facts_sorted(diags: &[crate::tsc::TypeAtFact]) -> bool {
  diags.windows(2).all(|pair| {
    (&pair[0].file, pair[0].offset, &pair[0].type_str)
      <= (&pair[1].file, pair[1].offset, &pair[1].type_str)
  })
}

fn lint_strict_native_baseline(raw: &str) -> Vec<String> {
  let baseline: StrictNativeBaseline = match serde_json::from_str(raw) {
    Ok(baseline) => baseline,
    Err(err) => {
      return vec![format!(
        "failed to parse as strict-native baseline schema: {err}"
      )];
    }
  };

  let mut errors = Vec::new();

  if baseline.schema_version != STRICT_NATIVE_BASELINE_SCHEMA_VERSION {
    errors.push(format!(
      "schema_version mismatch (expected {STRICT_NATIVE_BASELINE_SCHEMA_VERSION}, got {})",
      baseline.schema_version
    ));
  }

  let mut expected = baseline.diagnostics.clone();
  sort_diagnostics(&mut expected);
  if expected != baseline.diagnostics {
    errors.push("diagnostics are not in canonical sorted order".to_string());
  }

  errors
}
