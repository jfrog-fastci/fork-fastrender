use std::fs;
use std::path::{Path, PathBuf};
use typecheck_ts::tsconfig;
use typecheck_ts::lib_support::{CompilerOptions, JsxMode};

pub fn effective_type_packages(
  cfg: &tsconfig::ProjectConfig,
  options: &CompilerOptions,
  type_roots: &[PathBuf],
) -> Vec<String> {
  // `typecheck-ts` core treats `CompilerOptions.types` the same way it treats
  // `/// <reference types="..." />`: it resolves each entry using the host's
  // `resolve` hook (with an `@types/*` fallback) and queues the resulting `.d.ts`
  // file as an ambient input.
  //
  // native-js-cli is responsible for implementing TypeScript's `typeRoots` semantics:
  // - if `compilerOptions.types` is present in tsconfig, use it as-is (plus
  //   `jsxImportSource` injection when needed)
  // - if `types` is omitted, include all packages present under `typeRoots`
  let mut types_override = cfg.types.clone();
  if matches!(
    options.jsx,
    Some(JsxMode::React | JsxMode::ReactJsx | JsxMode::ReactJsxdev | JsxMode::Preserve)
  ) {
    if let (Some(import_source), Some(types)) =
      (cfg.jsx_import_source.as_ref(), types_override.as_mut())
    {
      if !types.iter().any(|name| name == import_source) {
        types.push(import_source.clone());
      }
    }
  }

  let mut packages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
  match types_override {
    Some(types) => {
      for name in types {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
          packages.insert(trimmed.to_string());
        }
      }
    }
    None => {
      for root in type_roots {
        let entries = match fs::read_dir(root) {
          Ok(entries) => entries,
          Err(_) => continue,
        };
        for entry in entries.filter_map(|e| e.ok()) {
          let path = entry.path();
          let Ok(file_type) = entry.file_type() else {
            continue;
          };
          if !file_type.is_dir() {
            continue;
          }
          let name = entry.file_name().to_string_lossy().to_string();
          if name.starts_with('@') {
            if let Ok(children) = fs::read_dir(&path) {
              for child in children.filter_map(|e| e.ok()) {
                let child_path = child.path();
                let Ok(child_type) = child.file_type() else {
                  continue;
                };
                if !child_type.is_dir() {
                  continue;
                }
                let child_name = child.file_name().to_string_lossy().to_string();
                if type_package_entry(&child_path).is_some() {
                  packages.insert(format!("{name}/{child_name}"));
                }
              }
            }
          } else {
            if type_package_entry(&path).is_some() {
              packages.insert(name);
            }
          }
        }
      }
    }
  }

  packages.into_iter().collect()
}

pub fn resolve_at_types_entry(type_roots: &[PathBuf], specifier: &str) -> Option<PathBuf> {
  let package = specifier.strip_prefix("@types/")?;
  if package.is_empty() {
    return None;
  }
  for root in type_roots {
    let dir = root.join(package);
    if !dir.is_dir() {
      continue;
    }
    let entry = type_package_entry(&dir)?;
    return Some(entry.canonicalize().unwrap_or(entry));
  }
  None
}

pub fn default_type_roots(root_dir: &Path) -> Vec<PathBuf> {
  let mut roots = Vec::new();
  for ancestor in root_dir.ancestors() {
    let candidate = ancestor.join("node_modules").join("@types");
    if candidate.is_dir() {
      roots.push(candidate);
    }
  }
  roots
}

fn type_package_entry(dir: &Path) -> Option<PathBuf> {
  let pkg_json = dir.join("package.json");
  if pkg_json.is_file() {
    if let Ok(text) = fs::read_to_string(&pkg_json) {
      if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
        let fields = ["types", "typings"];
        for field in fields {
          if let Some(path) = json.get(field).and_then(|v| v.as_str()) {
            let candidate = dir.join(path);
            if candidate.is_file() {
              return Some(candidate);
            }
          }
        }
      }
    }
  }
  let index = dir.join("index.d.ts");
  index.is_file().then_some(index)
}
