use std::path::{Path, PathBuf};

use diagnostics::paths::normalize_ts_path;
use serde_json::Value;
use typecheck_ts::lib_support::{CompilerOptions, ModuleResolutionKind};
use typecheck_ts::resolve::{ModuleResolutionMode, ResolveFs, Resolver};
use typecheck_ts::FileKey;

use crate::resolution_trace::ResolutionTraceMode;
use crate::runner::HarnessFileSet;

fn starts_with_drive_letter(path: &str) -> bool {
  let bytes = path.as_bytes();
  bytes.len() >= 3
    && bytes[0].is_ascii_alphabetic()
    && bytes[1] == b':'
    && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn looks_like_source_path(specifier: &str) -> bool {
  // `/// <reference path="a.ts" />` style specifiers are relative paths but do
  // not require a leading `./`.
  const SUFFIXES: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".mts", ".d.mts", ".cts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs",
  ];
  SUFFIXES.iter().any(|suffix| specifier.ends_with(suffix))
}

#[derive(Clone)]
struct HarnessResolveFs {
  files: HarnessFileSet,
}

impl ResolveFs for HarnessResolveFs {
  fn is_file(&self, path: &Path) -> bool {
    let normalized = normalize_ts_path(&path.to_string_lossy());
    self.files.resolve_ref(&normalized).is_some()
  }

  fn is_dir(&self, path: &Path) -> bool {
    // The resolver relies on `is_dir` to decide when to consult `package.json`
    // or probe for `index.*` entrypoints. Our virtual filesystem only stores
    // files, so approximate "directory exists" by checking for files that the
    // resolver would read inside that directory.
    //
    // This is enough to support:
    // - package root resolution (`node_modules/pkg` -> `package.json` / `index.*`)
    // - `@types/*` packages used by `compilerOptions.types` / `/// <reference types>`
    let normalized = normalize_ts_path(&path.to_string_lossy());
    let mut prefix = normalized;
    if !prefix.ends_with('/') {
      prefix.push('/');
    }
    let base_len = prefix.len();

    prefix.push_str("package.json");
    if self.files.resolve_ref(&prefix).is_some() {
      return true;
    }

    for ext in typecheck_ts::resolve::DEFAULT_EXTENSIONS {
      prefix.truncate(base_len);
      prefix.push_str("index.");
      prefix.push_str(ext);
      if self.files.resolve_ref(&prefix).is_some() {
        return true;
      }
    }

    false
  }

  fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
    Some(PathBuf::from(normalize_ts_path(&path.to_string_lossy())))
  }

  fn read_to_string(&self, path: &Path) -> Option<String> {
    let normalized = normalize_ts_path(&path.to_string_lossy());
    let key = self.files.resolve_ref(&normalized)?;
    self.files.content(key).map(|content| content.to_string())
  }
}

pub(crate) fn harness_resolve_mode(compiler_options: &CompilerOptions) -> ResolutionTraceMode {
  match compiler_options.effective_module_resolution() {
    ModuleResolutionKind::Classic => ResolutionTraceMode::Classic,
    ModuleResolutionKind::Node10 => ResolutionTraceMode::Node10,
    ModuleResolutionKind::Node16 => ResolutionTraceMode::Node16,
    ModuleResolutionKind::NodeNext => ResolutionTraceMode::NodeNext,
    ModuleResolutionKind::Bundler => ResolutionTraceMode::Bundler,
  }
}

pub(crate) fn resolve_module_specifier(
  files: &HarnessFileSet,
  from: &FileKey,
  specifier: &str,
  compiler_options: &CompilerOptions,
) -> Option<FileKey> {
  if specifier.starts_with('/') || specifier.starts_with('\\') || specifier.starts_with("./") {
    // `typecheck_ts::resolve` already handles absolute/relative specifiers.
  } else if !specifier.starts_with("../")
    && !specifier.starts_with('#')
    && !specifier.contains('/')
    && !specifier.contains('\\')
    && (specifier.ends_with(".ts")
      || specifier.ends_with(".tsx")
      || specifier.ends_with(".d.ts")
      || specifier.ends_with(".mts")
      || specifier.ends_with(".d.mts")
      || specifier.ends_with(".cts")
      || specifier.ends_with(".d.cts")
      || specifier.ends_with(".js")
      || specifier.ends_with(".jsx")
      || specifier.ends_with(".mjs")
      || specifier.ends_with(".cjs"))
  {
    let parent = Path::new(from.as_str())
      .parent()
      .unwrap_or_else(|| Path::new("/"));
    let candidate = normalize_ts_path(&parent.join(specifier).to_string_lossy());
    if let Some(found) = files.resolve(&candidate) {
      return Some(found);
    }
  }

  let fs = HarnessResolveFs {
    files: files.clone(),
  };
  let mut resolve_options = compiler_options.effective_resolve_options();
  // TypeScript resolves `/// <reference types="..." />` and `compilerOptions.types`
  // through the `@types/*` lookup regardless of the `moduleResolution` setting.
  //
  // `typecheck-ts` core models that by mapping type packages to explicit
  // `@types/*` specifiers. Allow those `@types/*` specifiers to be resolved via
  // `node_modules/` even when running in Classic module resolution mode.
  if specifier.starts_with("@types/") && resolve_options.module_resolution == ModuleResolutionMode::Classic {
    resolve_options.node_modules = true;
  }
  let resolver = Resolver::with_fs(fs, resolve_options);
  let from_path = Path::new(from.as_str());
  let mut resolved = resolver.resolve(from_path, specifier);
  if resolved.is_none()
    && !specifier.starts_with("./")
    && !specifier.starts_with("../")
    && !specifier.starts_with('#')
    && !specifier.starts_with('/')
    && !specifier.starts_with('\\')
    && !starts_with_drive_letter(specifier)
    && looks_like_source_path(specifier)
  {
    let mut prefixed = String::with_capacity(2 + specifier.len());
    prefixed.push_str("./");
    prefixed.push_str(specifier);
    resolved = resolver.resolve(from_path, &prefixed);
  }
  let resolved = resolved?;
  let resolved = normalize_ts_path(&resolved.to_string_lossy());
  if resolved.ends_with("/package.json") {
    // `typecheck-ts` does not treat JSON files as source inputs, but package
    // metadata still needs to be readable by the resolver. Filter out
    // `package.json` from resolved module specifiers so the checker never tries
    // to parse it as TS/JS.
    return None;
  }
  files.resolve(&resolved)
}

fn type_package_entry(files: &HarnessFileSet, dir: &str) -> Option<FileKey> {
  let pkg_json = normalize_ts_path(&Path::new(dir).join("package.json").to_string_lossy());
  if let Some(key) = files.resolve_ref(&pkg_json) {
    if let Some(text) = files.content(key) {
      if let Ok(json) = serde_json::from_str::<Value>(&text) {
        let fields = ["types", "typings"];
        for field in fields {
          if let Some(path) = json.get(field).and_then(|v| v.as_str()) {
            let candidate = if path.starts_with('/')
              || path.starts_with('\\')
              || starts_with_drive_letter(path)
            {
              normalize_ts_path(path)
            } else {
              normalize_ts_path(&Path::new(dir).join(path).to_string_lossy())
            };
            if let Some(entry) = files.resolve(&candidate) {
              return Some(entry);
            }
          }
        }
      }
    }
  }

  let index = normalize_ts_path(&Path::new(dir).join("index.d.ts").to_string_lossy());
  files.resolve(&index)
}

/// Resolve `@types/*` specifiers using the configured `typeRoots`, matching TypeScript's behaviour.
///
/// When `typeRoots` is empty, the TypeScript compiler searches for
/// `<ancestor>/node_modules/@types/<pkg>` for each ancestor directory of the importing file.
pub(crate) fn resolve_at_types_entry(
  files: &HarnessFileSet,
  from: &FileKey,
  type_roots: &[String],
  specifier: &str,
) -> Option<FileKey> {
  let package = specifier.strip_prefix("@types/")?;
  if package.is_empty() {
    return None;
  }

  if !type_roots.is_empty() {
    for root in type_roots {
      let root = normalize_ts_path(root);
      let dir = normalize_ts_path(&Path::new(&root).join(package).to_string_lossy());
      if let Some(entry) = type_package_entry(files, &dir) {
        return Some(entry);
      }
    }
    return None;
  }

  let base_dir = Path::new(from.as_str())
    .parent()
    .unwrap_or_else(|| Path::new("/"));
  for ancestor in base_dir.ancestors() {
    let root = normalize_ts_path(&ancestor.join("node_modules").join("@types").to_string_lossy());
    let dir = normalize_ts_path(&Path::new(&root).join(package).to_string_lossy());
    if let Some(entry) = type_package_entry(files, &dir) {
      return Some(entry);
    }
  }

  None
}

#[cfg(test)]
mod tests {
  use super::harness_resolve_mode;
  use crate::resolution_trace::ResolutionTraceMode;
  use typecheck_ts::lib_support::{CompilerOptions, ModuleKind, ScriptTarget};

  #[test]
  fn harness_resolve_mode_defaults_match_tsc() {
    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    options.module = None;
    assert_eq!(harness_resolve_mode(&options), ResolutionTraceMode::Bundler);

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    options.module = None;
    assert_eq!(harness_resolve_mode(&options), ResolutionTraceMode::Bundler);

    for module in [ModuleKind::None, ModuleKind::Amd, ModuleKind::Umd, ModuleKind::System] {
      let mut options = CompilerOptions::default();
      options.module = Some(module);
      assert_eq!(
        harness_resolve_mode(&options),
        ResolutionTraceMode::Classic,
        "expected module={module:?} to default to Classic resolution"
      );
    }

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::Node16);
    assert_eq!(harness_resolve_mode(&options), ResolutionTraceMode::Node16);

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::NodeNext);
    assert_eq!(
      harness_resolve_mode(&options),
      ResolutionTraceMode::NodeNext
    );
  }
}
