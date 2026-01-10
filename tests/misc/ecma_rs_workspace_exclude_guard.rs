//! Guard against accidentally pulling the vendored `ecma-rs` workspace into the FastRender workspace.
//!
//! FastRender vendors `ecma-rs` under `vendor/ecma-rs/`, which is itself a nested Cargo workspace.
//! The root `Cargo.toml` must keep that directory in `[workspace].exclude` so Cargo resolves
//! `vm-js`/`webidl` dependencies against their own workspace metadata (and so future updates don't
//! accidentally balloon the FastRender workspace).

use std::fs;
use std::path::PathBuf;

#[test]
fn root_workspace_excludes_vendored_ecma_rs() {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let vendor_path = repo_root.join("vendor").join("ecma-rs");
  assert!(
    vendor_path.exists(),
    "expected vendored ecma-rs workspace at {}",
    vendor_path.display()
  );

  let manifest_path = repo_root.join("Cargo.toml");
  let manifest_src = fs::read_to_string(&manifest_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
  let manifest: toml::Value = manifest_src
    .parse()
    .unwrap_or_else(|err| panic!("failed to parse {} as TOML: {err}", manifest_path.display()));

  let workspace = manifest
    .get("workspace")
    .and_then(|value| value.as_table())
    .expect("Cargo.toml must contain a [workspace] table");

  let exclude = workspace
    .get("exclude")
    .and_then(|value| value.as_array())
    .expect("Cargo.toml must define [workspace].exclude as an array");

  assert!(
    exclude.iter().any(|entry| entry.as_str() == Some("vendor/ecma-rs")),
    "Cargo.toml [workspace].exclude must contain \"vendor/ecma-rs\" so Cargo treats it as a nested workspace"
  );

  for key in ["members", "default-members"] {
    if let Some(list) = workspace.get(key).and_then(|value| value.as_array()) {
      for member in list.iter().filter_map(|value| value.as_str()) {
        assert!(
          !member.starts_with("vendor/ecma-rs"),
          "Cargo.toml [workspace].{key} must not include vendored ecma-rs members (found {member:?})"
        );
      }
    }
  }
}

