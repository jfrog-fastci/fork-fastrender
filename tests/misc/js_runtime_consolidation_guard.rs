//! Guards for JavaScript runtime consolidation decisions.
//!
//! FastRender is migrating to the `vm-js` + WebIDL bindings pipeline as the default JavaScript
//! execution path. Legacy QuickJS-based crates (`js-dom-bindings*`) are still kept in the repository
//! for comparison/debugging, but they must not be pulled into *default* workspace builds.

use std::fs;
use std::path::PathBuf;

const LEGACY_QUICKJS_CRATES: [&str; 2] =
  ["crates/js-dom-bindings", "crates/js-dom-bindings-quickjs"];

fn parse_root_manifest() -> toml::Value {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let manifest_path = repo_root.join("Cargo.toml");
  let contents = fs::read_to_string(&manifest_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
  toml::from_str(&contents)
    .unwrap_or_else(|err| panic!("failed to parse {}: {err}", manifest_path.display()))
}

fn toml_array_contains_str(array: &[toml::Value], needle: &str) -> bool {
  array.iter().any(|v| v.as_str() == Some(needle))
}

#[test]
fn legacy_quickjs_crates_are_excluded_from_default_workspace_builds() {
  let manifest = parse_root_manifest();
  let workspace = manifest
    .get("workspace")
    .and_then(|v| v.as_table())
    .expect("root Cargo.toml must define [workspace]");

  let members = workspace
    .get("members")
    .and_then(|v| v.as_array())
    .expect("[workspace] must define members as an array");
  let has_legacy_members = LEGACY_QUICKJS_CRATES
    .iter()
    .any(|legacy| toml_array_contains_str(members, legacy));

  match workspace.get("default-members").and_then(|v| v.as_array()) {
    Some(default_members) => {
      for legacy in LEGACY_QUICKJS_CRATES {
        assert!(
          !toml_array_contains_str(default_members, legacy),
          "legacy QuickJS crate `{legacy}` must not be included in [workspace].default-members"
        );
      }
    }
    None => {
      // If `default-members` is removed entirely, the only safe option is to remove the legacy
      // crates from the workspace membership list so `cargo build/test` doesn't pull them in by
      // default.
      assert!(
        !has_legacy_members,
        "workspace includes legacy QuickJS crates, but [workspace].default-members is not set. \
         Either remove legacy crates from [workspace].members or keep them excluded via default-members."
      );
    }
  }
}

#[test]
fn quickjs_feature_is_not_enabled_by_default() {
  let manifest = parse_root_manifest();
  let features = manifest.get("features").and_then(|v| v.as_table());
  let Some(features) = features else {
    return;
  };

  let default_features = features
    .get("default")
    .and_then(|v| v.as_array())
    .cloned()
    .unwrap_or_default();
  assert!(
    !toml_array_contains_str(&default_features, "quickjs"),
    "`quickjs` must not be part of the default feature set"
  );
}

#[test]
fn rquickjs_dependency_is_optional_when_present() {
  let manifest = parse_root_manifest();
  let dependencies = manifest.get("dependencies").and_then(|v| v.as_table());
  let Some(dependencies) = dependencies else {
    return;
  };

  let Some(rquickjs) = dependencies.get("rquickjs") else {
    // Removing the dependency entirely is also acceptable.
    return;
  };
  let rquickjs = rquickjs
    .as_table()
    .expect("dependencies.rquickjs must be a table when present");
  assert_eq!(
    rquickjs.get("optional").and_then(|v| v.as_bool()),
    Some(true),
    "dependencies.rquickjs must remain optional so default builds do not pull in QuickJS"
  );
}
