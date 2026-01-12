//! Guard against re-introducing stale references to the pre-consolidation WebIDL crate layout.
//!
//! WebIDL consolidation removed the old workspace-local WebIDL crates that lived under `crates/`.
//! References to those old paths in docs/scripts/source comments are misleading and tend to
//! resurrect the old ownership model over time.
//!
//! This guard checks the parts of the repo where contributors are most likely to add such
//! references (source, scripts, and contributor-facing docs). It intentionally does *not* scan the
//! `instructions/` directory, which may mention the old layout for historical/migration context.
//!
//! Note: We also intentionally do not walk `vendor/` or `specs/` (very large) and avoid scanning
//! fixture data in `tests/` (also large). The goal here is to prevent regressions in
//! contributor-facing locations, not to police third-party content.

use std::fs;
use std::path::{Path, PathBuf};

const CRATES_DIR: &str = "crates";
const WEBIDL_CRATE_PREFIX: &str = "webidl-";
const DELETED_WEBIDL_CRATE_SUFFIXES: [&str; 4] = ["ir", "bindings-core", "vm-js", "js-runtime"];

#[test]
fn no_stale_webidl_crates_paths_in_repo() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

  let search_roots = [
    repo_root.join("src"),
    repo_root.join("scripts"),
    repo_root.join("docs"),
    repo_root.join("xtask"),
    repo_root.join("tests").join("guards"),
    repo_root.join("Cargo.toml"),
  ];

  let mut offenders = Vec::new();

  for root in search_roots {
    if !root.exists() {
      continue;
    }

    if root.is_dir() {
      for path in text_files(&root) {
        scan_file(&repo_root, &path, &mut offenders);
      }
    } else {
      scan_file(&repo_root, &root, &mut offenders);
    }
  }

  assert!(
    offenders.is_empty(),
    "found stale references to removed workspace-local WebIDL crates. After WebIDL consolidation, the\n\
     canonical WebIDL stack lives under `vendor/ecma-rs/webidl*`.\n\
     \n\
     Offenders:\n\
     {}",
    offenders.join("\n")
  );
}

fn scan_file(repo_root: &Path, path: &Path, offenders: &mut Vec<String>) {
  // Only scan known text file types to avoid spurious UTF-8 errors and to keep runtime bounded.
  if !is_text_file(path) {
    return;
  }

  let Ok(contents) = fs::read_to_string(path) else {
    return;
  };

  let rel_path = path
    .strip_prefix(repo_root)
    .map(|p| p.display().to_string())
    .unwrap_or_else(|_| path.display().to_string());

  for (line_idx, line) in contents.lines().enumerate() {
    let line_number = line_idx + 1;
    for suffix in DELETED_WEBIDL_CRATE_SUFFIXES {
      let pattern = format!("{CRATES_DIR}/{WEBIDL_CRATE_PREFIX}{suffix}");
      if line.contains(&pattern) {
        offenders.push(format!("{rel_path}:{line_number}: contains {pattern:?}"));
      }
    }
  }
}

fn is_text_file(path: &Path) -> bool {
  let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
    return false;
  };

  matches!(
    ext,
    "rs" | "md" | "toml" | "sh" | "yml" | "yaml" | "txt" | "json"
  )
}

fn text_files(root: &Path) -> Vec<PathBuf> {
  let mut files = Vec::new();
  let mut stack = vec![root.to_path_buf()];

  while let Some(dir) = stack.pop() {
    let entries = fs::read_dir(&dir)
      .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", dir.display()));

    for entry in entries {
      let entry = entry
        .unwrap_or_else(|err| panic!("failed to read entry under {}: {err}", dir.display()));
      let path = entry.path();

      if path.is_dir() {
        stack.push(path);
      } else if is_text_file(&path) {
        files.push(path);
      }
    }
  }

  files.sort();
  files
}
