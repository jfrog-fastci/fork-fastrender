use std::path::{Path, PathBuf};

use diagnostics::paths::normalize_ts_path;
use typecheck_ts::lib_support::{CompilerOptions, ModuleKind};
use typecheck_ts::resolve::{ModuleResolutionMode, ResolveFs, ResolveOptions, Resolver};
use typecheck_ts::FileKey;

use crate::runner::HarnessFileSet;

fn resolve_options_for_compiler_options(compiler_options: &CompilerOptions) -> ResolveOptions {
  let module_resolution = module_resolution_from_compiler_options(compiler_options);
  let (node_modules, package_imports) = match module_resolution {
    ModuleResolutionMode::Classic => (false, false),
    // TypeScript's legacy `node` resolver does not support `package.json` imports maps.
    ModuleResolutionMode::Node10 => (true, false),
    // Node16/NodeNext/Bundler support `package.json` exports/imports maps.
    ModuleResolutionMode::Node16 | ModuleResolutionMode::NodeNext | ModuleResolutionMode::Bundler => (true, true),
  };

  ResolveOptions {
    node_modules,
    package_imports,
    module_resolution,
    module_kind: compiler_options.module,
    ..Default::default()
  }
}

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

  fn is_dir(&self, _path: &Path) -> bool {
    // The resolver doesn't currently consult directories; keep this conservative.
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
  let mut resolve_options = resolve_options_for_compiler_options(compiler_options);
  // TypeScript resolves `/// <reference types="..." />` and `compilerOptions.types`
  // through the `@types/*` lookup regardless of the `moduleResolution` setting.
  //
  // `typecheck-ts` owns the `@types/*` fallback mapping, but hosts still need to
  // resolve `@types/*` specifiers via node_modules. Enable that narrow case even
  // when running in Classic module resolution mode.
  if !resolve_options.node_modules && specifier.starts_with("@types/") {
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

fn module_resolution_from_compiler_options(options: &CompilerOptions) -> ModuleResolutionMode {
  if let Some(raw) = options.module_resolution.as_deref() {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
      return parse_module_resolution_mode(trimmed).unwrap_or(ModuleResolutionMode::Classic);
    }
  }
  infer_default_module_resolution_mode(options.module)
}

fn parse_module_resolution_mode(raw: &str) -> Option<ModuleResolutionMode> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "classic" => Some(ModuleResolutionMode::Classic),
    // TypeScript treats `node` as the legacy Node10 resolver.
    "node" | "nodejs" | "node10" => Some(ModuleResolutionMode::Node10),
    "node16" => Some(ModuleResolutionMode::Node16),
    "nodenext" => Some(ModuleResolutionMode::NodeNext),
    "bundler" => Some(ModuleResolutionMode::Bundler),
    _ => None,
  }
}

fn infer_default_module_resolution_mode(module: Option<ModuleKind>) -> ModuleResolutionMode {
  // Best-effort mirror of `tsc`'s default `moduleResolution` selection when the
  // option is not explicitly specified.
  //
  // TypeScript's exact defaults also depend on other flags (notably `target`),
  // but the harness only tracks the `module` option today.
  match module {
    Some(ModuleKind::Node16) => ModuleResolutionMode::Node16,
    Some(ModuleKind::NodeNext) => ModuleResolutionMode::NodeNext,
    Some(ModuleKind::CommonJs) => ModuleResolutionMode::Node10,
    _ => ModuleResolutionMode::Classic,
  }
}
