use crate::tsconfig;
use diagnostics::paths::normalize_fs_path;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, JsxMode, LibFile};
use typecheck_ts::FileKey;

pub fn load_type_libs(
  cfg: &tsconfig::ProjectConfig,
  options: &CompilerOptions,
  type_roots: &[PathBuf],
) -> Result<Vec<LibFile>, String> {
  let mut libs = Vec::new();
  if type_roots.is_empty() {
    return Ok(ensure_placeholder_libs(libs, options));
  }

  let mut types_override = cfg.types.clone();
  if matches!(
    options.jsx,
    Some(JsxMode::React | JsxMode::ReactJsx | JsxMode::ReactJsxdev | JsxMode::Preserve)
  ) {
    if let (Some(import_source), Some(types)) = (cfg.jsx_import_source.as_ref(), types_override.as_mut())
    {
      if !types.iter().any(|name| name == import_source) {
        types.push(import_source.clone());
        types.sort();
        types.dedup();
      }
    }
  }

  if let Some(types) = types_override.as_ref() {
    for name in types {
      let Some(dir) = resolve_type_package(type_roots, name) else {
        return Err(format!(
          "failed to resolve type definition package '{name}' from {}",
          cfg.root_dir.display()
        ));
      };
      if let Some(lib) = lib_file_from_type_package(name, &dir)? {
        libs.push(lib);
      }
    }
  } else {
    // No explicit `types` list: include all packages in the type roots.
    let mut packages: BTreeMap<String, PathBuf> = BTreeMap::new();
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
              let scoped = format!("{name}/{child_name}");
              packages.entry(scoped).or_insert(child_path);
            }
          }
        } else {
          packages.entry(name).or_insert(path);
        }
      }
    }

    for (name, dir) in packages {
      if let Some(lib) = lib_file_from_type_package(&name, &dir)? {
        libs.push(lib);
      }
    }
  }

  Ok(ensure_placeholder_libs(libs, options))
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

fn ensure_placeholder_libs(mut libs: Vec<LibFile>, options: &CompilerOptions) -> Vec<LibFile> {
  if !libs.is_empty() || !options.no_default_lib {
    return libs;
  }
  // `typecheck-ts` emits an error diagnostic when zero libs are loaded. Mirror `tsc --noLib`
  // by injecting a single empty `.d.ts` placeholder so the program can proceed without
  // default libs.
  libs.push(LibFile {
    key: FileKey::new("lib:empty.d.ts"),
    name: Arc::from("empty.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(""),
  });
  libs
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

fn resolve_type_package(type_roots: &[PathBuf], package: &str) -> Option<PathBuf> {
  for root in type_roots {
    let dir = root.join(package);
    if dir.is_dir() {
      return Some(dir);
    }
    if let Some(encoded) = encode_types_package_name(package) {
      let dir = root.join(encoded);
      if dir.is_dir() {
        return Some(dir);
      }
    }
  }
  None
}

fn encode_types_package_name(package: &str) -> Option<String> {
  let (scope, name) = package.split_once('/')?;
  if !scope.starts_with('@') || name.is_empty() {
    return None;
  }
  let scope = scope.trim_start_matches('@');
  Some(format!("{scope}__{name}"))
}

fn lib_file_from_type_package(package: &str, dir: &Path) -> Result<Option<LibFile>, String> {
  let entry = match type_package_entry(dir) {
    Some(path) => path,
    None => return Ok(None),
  };
  let canonical = entry.canonicalize().unwrap_or(entry.clone());
  let text = fs::read_to_string(&canonical).map_err(|err| {
    format!(
      "failed to read type definitions {}: {err}",
      canonical.display()
    )
  })?;
  Ok(Some(LibFile {
    key: FileKey::new(normalize_fs_path(&canonical)),
    name: Arc::from(format!("types:{package}")),
    kind: FileKind::Dts,
    text: Arc::from(text),
  }))
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

