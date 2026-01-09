//! WebIDL loader helpers.
//!
//! This module is responsible for building the combined IDL input used by codegen/report tooling.
//!
//! Deterministic order:
//! 1. `tools/webidl/prelude.idl`
//! 2. `tools/webidl/overrides/*.idl` (lexicographic by file name)
//! 3. Spec sources (Bikeshed `*.bs`, WHATWG HTML `source`), in the caller-provided order
//!
//! Prelude/overrides are concatenated *before* spec sources so specs can override these
//! definitions if needed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::extract_webidl_blocks;

pub const WEBIDL_PRELUDE_REL_PATH: &str = "tools/webidl/prelude.idl";
pub const WEBIDL_OVERRIDES_REL_DIR: &str = "tools/webidl/overrides";

#[derive(Debug, Clone, Copy)]
pub struct WebIdlSource<'a> {
  pub rel_path: &'a str,
  pub label: &'a str,
}

#[derive(Debug, Clone)]
pub struct LoadCombinedWebIdlResult {
  pub combined_idl: String,
  /// Sources that were requested but not found on disk (e.g. missing git submodules).
  pub missing_sources: Vec<(String, PathBuf)>,
}

/// Load the WebIDL prelude + overrides + requested spec sources, returning a single IDL string.
///
/// Missing spec sources are recorded in [`LoadCombinedWebIdlResult::missing_sources`] instead of
/// erroring so callers can decide whether to skip tests / degrade gracefully when spec submodules
/// are not checked out.
pub fn load_combined_webidl(
  repo_root: &Path,
  sources: &[WebIdlSource<'_>],
) -> Result<LoadCombinedWebIdlResult> {
  let mut parts: Vec<String> = Vec::new();

  // Prelude.
  let prelude_path = repo_root.join(WEBIDL_PRELUDE_REL_PATH);
  let prelude = std::fs::read_to_string(&prelude_path)
    .with_context(|| format!("read WebIDL prelude {}", prelude_path.display()))?;
  parts.push(prelude);

  // Overrides.
  let overrides_dir = repo_root.join(WEBIDL_OVERRIDES_REL_DIR);
  if overrides_dir.is_dir() {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&overrides_dir)
      .with_context(|| format!("read WebIDL overrides dir {}", overrides_dir.display()))?
      .filter_map(|e| e.ok())
      .map(|e| e.path())
      .filter(|p| p.is_file() && p.extension().and_then(|ext| ext.to_str()) == Some("idl"))
      .collect();
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for path in entries {
      let src = std::fs::read_to_string(&path).with_context(|| format!("read override {}", path.display()))?;
      parts.push(src);
    }
  }

  // Spec sources.
  let mut missing = Vec::new();
  for source in sources {
    let path = repo_root.join(source.rel_path);
    if !path.exists() {
      missing.push((source.label.to_string(), path));
      continue;
    }
    let src = std::fs::read_to_string(&path)
      .with_context(|| format!("read spec source {} ({})", source.label, path.display()))?;
    for block in extract_webidl_blocks(&src) {
      parts.push(block);
    }
  }

  Ok(LoadCombinedWebIdlResult {
    combined_idl: parts.join(super::WEBIDL_BLOCK_SEPARATOR),
    missing_sources: missing,
  })
}
