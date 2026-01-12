use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn bundled_version_or_skip() -> Option<&'static str> {
  match typecheck_ts::lib_support::bundled_typescript_version() {
    Some(version) => Some(version),
    None => {
      eprintln!(
        "skipping TypeScript version pin checks: typecheck-ts built without bundled-libs"
      );
      None
    }
  }
}

fn read_json(path: &Path) -> Value {
  let text = fs::read_to_string(path)
    .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
  serde_json::from_str(&text)
    .unwrap_or_else(|err| panic!("parse JSON {}: {err}", path.display()))
}

#[test]
fn baselines_pin_typescript_version() {
  let Some(pinned) = bundled_version_or_skip() else {
    return;
  };

  let baselines_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines");
  let mut json_files: Vec<PathBuf> = WalkDir::new(&baselines_root)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| entry.into_path())
    .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("json"))
    .collect();
  json_files.sort();

  let mut missing_version = Vec::new();
  let mut mismatches = Vec::new();
  for path in json_files {
    let value = read_json(&path);
    let Some(metadata) = value.get("metadata") else {
      // Not a tsc baseline (e.g. strict-native baselines).
      continue;
    };
    let Some(metadata) = metadata.as_object() else {
      missing_version.push(format!("{}: metadata is not an object", path.display()));
      continue;
    };

    let found = metadata
      .get("typescript_version")
      .or_else(|| metadata.get("typescriptVersion"))
      .and_then(|v| v.as_str());
    let Some(found) = found else {
      missing_version.push(format!(
        "{}: missing metadata.typescript_version",
        path.display()
      ));
      continue;
    };
    if found != pinned {
      mismatches.push(format!("{}: {found} != {pinned}", path.display()));
    }

    let bundled = metadata
      .get("bundled_typescript_version")
      .or_else(|| metadata.get("bundledTypescriptVersion"))
      .and_then(|v| v.as_str());
    if let Some(bundled) = bundled {
      if bundled != pinned {
        mismatches.push(format!(
          "{}: bundled_typescript_version {bundled} != {pinned}",
          path.display()
        ));
      }
    }
  }

  assert!(
    missing_version.is_empty(),
    "some baselines are missing TypeScript version metadata:\n{}",
    missing_version.join("\n")
  );
  assert!(
    mismatches.is_empty(),
    "some baselines were generated with a different TypeScript version:\n{}",
    mismatches.join("\n")
  );
}

#[test]
fn harness_node_packages_pin_typescript_version() {
  let Some(pinned) = bundled_version_or_skip() else {
    return;
  };

  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

  let pkg_json = read_json(&manifest_dir.join("package.json"));
  let pkg_version = pkg_json
    .get("dependencies")
    .and_then(|v| v.get("typescript"))
    .and_then(|v| v.as_str())
    .unwrap_or("<missing>");
  assert_eq!(
    pkg_version,
    pinned,
    "typecheck-ts-harness/package.json pins typescript@{pkg_version}, but bundled libs are {pinned}"
  );

  let lock_json = read_json(&manifest_dir.join("package-lock.json"));
  let packages = lock_json
    .get("packages")
    .and_then(|v| v.as_object())
    .expect("package-lock.json missing packages map");
  let root_dep = packages
    .get("")
    .and_then(|v| v.get("dependencies"))
    .and_then(|v| v.get("typescript"))
    .and_then(|v| v.as_str())
    .unwrap_or("<missing>");
  let node_module = packages
    .get("node_modules/typescript")
    .and_then(|v| v.get("version"))
    .and_then(|v| v.as_str())
    .unwrap_or("<missing>");
  assert_eq!(
    root_dep,
    pinned,
    "typecheck-ts-harness/package-lock.json root pins typescript@{root_dep}, but bundled libs are {pinned}"
  );
  assert_eq!(
    node_module,
    pinned,
    "typecheck-ts-harness/package-lock.json installs typescript@{node_module}, but bundled libs are {pinned}"
  );
}

