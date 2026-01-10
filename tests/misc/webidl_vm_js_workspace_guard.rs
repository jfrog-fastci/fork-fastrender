//! Guard against accidentally depending on both the workspace-local and vendored `webidl-vm-js`.
//!
//! FastRender keeps a workspace-local copy of `webidl-vm-js` at `crates/webidl-vm-js`. The vendored
//! `ecma-rs` copy (`vendor/ecma-rs/webidl-vm-js`) should not be pulled into the FastRender workspace
//! dependency graph to avoid ambiguity/divergence.

use std::fs;
use std::path::{Path, PathBuf};

const VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT: &str = "vendor/ecma-rs/webidl-vm-js";

#[test]
fn no_workspace_cargo_toml_depends_on_vendored_webidl_vm_js() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_paths = cargo_toml_files(&repo_root);

  let mut offenders = Vec::new();
  for path in manifest_paths {
    let contents = fs::read_to_string(&path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    if contents.contains(VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT) {
      let rel_path = path
        .strip_prefix(&repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
      offenders.push(rel_path);
    }
  }

  if !offenders.is_empty() {
    panic!(
      "FastRender must not depend on the vendored `webidl-vm-js` crate ({VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT}).\n\
       Use the workspace-local adapter at `crates/webidl-vm-js`.\n\
       \n\
       Offending Cargo.toml files:\n\
       {}",
      offenders.join("\n")
    );
  }
}

#[test]
fn cargo_lock_contains_only_one_webidl_vm_js_package() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let lock_path = repo_root.join("Cargo.lock");
  let lockfile = fs::read_to_string(&lock_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", lock_path.display()));

  let count = lockfile.matches("name = \"webidl-vm-js\"").count();
  assert_eq!(
    count, 1,
    "expected exactly one `webidl-vm-js` package in Cargo.lock, found {count}"
  );
}

fn cargo_toml_files(repo_root: &Path) -> Vec<PathBuf> {
  // Only scan manifests that are part of the FastRender workspace/tooling. Avoid walking the full
  // repo tree (spec submodules are very large).
  let mut files = Vec::new();

  let root_manifest = repo_root.join("Cargo.toml");
  if root_manifest.exists() {
    files.push(root_manifest);
  }

  let xtask_manifest = repo_root.join("xtask").join("Cargo.toml");
  if xtask_manifest.exists() {
    files.push(xtask_manifest);
  }

  let fuzz_manifest = repo_root.join("fuzz").join("Cargo.toml");
  if fuzz_manifest.exists() {
    files.push(fuzz_manifest);
  }

  let crates_dir = repo_root.join("crates");
  if let Ok(entries) = fs::read_dir(&crates_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if !path.is_dir() {
        continue;
      }
      let manifest = path.join("Cargo.toml");
      if manifest.exists() {
        files.push(manifest);
      }
    }
  }

  files.sort();
  files
}
