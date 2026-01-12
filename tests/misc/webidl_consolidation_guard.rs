//! Guard against regressing the post-consolidation WebIDL workspace layout.
//!
//! After `instructions/webidl_consolidation.md` is complete, FastRender must only depend on the
//! vendored `ecma-rs` WebIDL stack (`vendor/ecma-rs/webidl*`). The workspace-local WebIDL crates
//! under the repo's `crates/` directory are expected to be deleted.

use std::fs;
use std::path::{Path, PathBuf};

const CRATES_DIR: &str = "crates";
const WEBIDL_CRATE_PREFIX: &str = "webidl-";
const DELETED_WEBIDL_CRATE_SUFFIXES: [&str; 4] = ["ir", "bindings-core", "vm-js", "js-runtime"];

const VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT: &str = "vendor/ecma-rs/webidl-vm-js";

#[test]
fn no_workspace_cargo_toml_references_deleted_webidl_crates() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_paths = cargo_toml_files(&repo_root);

  let mut offenders = Vec::new();
  for path in manifest_paths {
    let contents = fs::read_to_string(&path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    for suffix in DELETED_WEBIDL_CRATE_SUFFIXES {
      let fragment = format!("{CRATES_DIR}/{WEBIDL_CRATE_PREFIX}{suffix}");
      if contents.contains(&fragment) {
        offenders.push(format!(
          "{} contains {fragment:?}",
          display_repo_relative(&repo_root, &path)
        ));
      }
    }
  }

  assert!(
    offenders.is_empty(),
    "FastRender's Cargo manifests must not reference deleted workspace-local WebIDL crates.\n\
     After consolidation, these crates live under `vendor/ecma-rs/` only.\n\
     \n\
     Offenders:\n\
     {}",
    offenders.join("\n")
  );
}

#[test]
fn workspace_does_not_list_deleted_webidl_crates() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_path = repo_root.join("Cargo.toml");
  let manifest_src = fs::read_to_string(&manifest_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
  let manifest: toml::Value = manifest_src
    .parse()
    .unwrap_or_else(|err| panic!("failed to parse {} as TOML: {err}", manifest_path.display()));

  let workspace = manifest
    .get("workspace")
    .and_then(|value| value.as_table())
    .expect("root Cargo.toml must contain a [workspace] table");

  for key in ["members", "default-members"] {
    let Some(list) = workspace.get(key).and_then(|value| value.as_array()) else {
      continue;
    };
    for member in list.iter().filter_map(|value| value.as_str()) {
      for suffix in DELETED_WEBIDL_CRATE_SUFFIXES {
        let fragment = format!("{CRATES_DIR}/{WEBIDL_CRATE_PREFIX}{suffix}");
        assert!(
          !member.contains(&fragment),
          "root Cargo.toml [workspace].{key} must not include deleted WebIDL crates (found {member:?})"
        );
      }
    }
  }
}

#[test]
fn workspace_uses_vendored_webidl_vm_js() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_path = repo_root.join("Cargo.toml");
  let manifest_src = fs::read_to_string(&manifest_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
  let manifest: toml::Value = manifest_src
    .parse()
    .unwrap_or_else(|err| panic!("failed to parse {} as TOML: {err}", manifest_path.display()));

  let root_dependencies = manifest.get("dependencies").and_then(|value| value.as_table());
  let workspace_dependencies = manifest
    .get("workspace")
    .and_then(|value| value.as_table())
    .and_then(|workspace| workspace.get("dependencies"))
    .and_then(|value| value.as_table());

  let root_dep_path = root_dependencies
    .and_then(|deps| deps.get("webidl-vm-js"))
    .and_then(dep_path);
  let workspace_dep_path = workspace_dependencies
    .and_then(|deps| deps.get("webidl-vm-js"))
    .and_then(dep_path);

  assert!(
    root_dep_path == Some(VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT)
      || workspace_dep_path == Some(VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT),
    "after consolidation, the workspace must depend on vendored `webidl-vm-js` at {VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT}.\n\
     Set either:\n\
       - [workspace.dependencies].webidl-vm-js.path = {VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT:?}\n\
       - [dependencies].webidl-vm-js.path = {VENDORED_WEBIDL_VM_JS_PATH_FRAGMENT:?}\n\
     \n\
     Observed:\n\
       - Cargo.toml [dependencies].webidl-vm-js.path = {root_dep_path:?}\n\
       - Cargo.toml [workspace.dependencies].webidl-vm-js.path = {workspace_dep_path:?}"
  );
}

#[test]
fn cargo_lock_contains_only_one_webidl_stack() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let lock_path = repo_root.join("Cargo.lock");
  let lockfile = fs::read_to_string(&lock_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", lock_path.display()));

  let webidl_count = lockfile.matches("name = \"webidl\"").count();
  assert_eq!(
    webidl_count, 1,
    "expected exactly one `webidl` package in Cargo.lock, found {webidl_count}"
  );

  let webidl_vm_js_count = lockfile.matches("name = \"webidl-vm-js\"").count();
  assert_eq!(
    webidl_vm_js_count, 1,
    "expected exactly one `webidl-vm-js` package in Cargo.lock, found {webidl_vm_js_count}"
  );

  let webidl_js_runtime_count = lockfile.matches("name = \"webidl-js-runtime\"").count();
  let webidl_runtime_count = lockfile.matches("name = \"webidl-runtime\"").count();

  assert!(
    webidl_js_runtime_count == 0 || webidl_runtime_count == 0,
    "Cargo.lock must not contain both `webidl-js-runtime` and `webidl-runtime` packages.\n\
     Found webidl-js-runtime={webidl_js_runtime_count}, webidl-runtime={webidl_runtime_count}"
  );

  if webidl_js_runtime_count > 0 {
    assert_eq!(
      webidl_js_runtime_count, 1,
      "expected at most one `webidl-js-runtime` package in Cargo.lock, found {webidl_js_runtime_count}"
    );
  }
  if webidl_runtime_count > 0 {
    assert_eq!(
      webidl_runtime_count, 1,
      "expected at most one `webidl-runtime` package in Cargo.lock, found {webidl_runtime_count}"
    );
  }
}

fn dep_path(dep: &toml::Value) -> Option<&str> {
  dep.as_table()?.get("path")?.as_str()
}

fn display_repo_relative(repo_root: &Path, path: &Path) -> String {
  path
    .strip_prefix(repo_root)
    .map(|p| p.display().to_string())
    .unwrap_or_else(|_| path.display().to_string())
}

fn cargo_toml_files(repo_root: &Path) -> Vec<PathBuf> {
  // Only scan manifests that are part of the FastRender workspace/tooling. Avoid walking the full
  // repo tree (`vendor/` and spec submodules are very large).
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
