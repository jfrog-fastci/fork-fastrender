use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use typecheck_ts::lib_support::{CompilerOptions, JsxMode, LibName, ModuleKind, ScriptTarget};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct ProjectConfig {
  pub root_dir: PathBuf,
  pub compiler_options: CompilerOptions,
  pub base_url: Option<PathBuf>,
  pub paths: BTreeMap<String, Vec<String>>,
  pub root_files: Vec<PathBuf>,
  /// Raw `compilerOptions.types` list (distinguishes between unset and empty).
  pub types: Option<Vec<String>>,
  /// Raw `compilerOptions.typeRoots` list, resolved to absolute paths.
  pub type_roots: Option<Vec<PathBuf>>,
  pub jsx_import_source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTsConfig {
  #[serde(default)]
  extends: Option<String>,
  #[serde(default)]
  compiler_options: RawCompilerOptions,
  #[serde(default)]
  files: Option<Vec<String>>,
  #[serde(default)]
  include: Option<Vec<String>>,
  #[serde(default)]
  exclude: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCompilerOptions {
  #[serde(default)]
  target: Option<String>,
  #[serde(default)]
  module: Option<String>,
  #[serde(default)]
  lib: Option<Vec<String>>,
  #[serde(default)]
  types: Option<Vec<String>>,
  #[serde(default)]
  type_roots: Option<Vec<String>>,
  #[serde(default)]
  module_resolution: Option<String>,
  #[serde(default)]
  skip_lib_check: Option<bool>,
  #[serde(default)]
  no_emit: Option<bool>,
  #[serde(default)]
  no_emit_on_error: Option<bool>,
  #[serde(default)]
  declaration: Option<bool>,
  #[serde(default)]
  strict: Option<bool>,
  #[serde(default)]
  no_implicit_any: Option<bool>,
  #[serde(default)]
  strict_null_checks: Option<bool>,
  #[serde(default)]
  strict_function_types: Option<bool>,
  #[serde(default)]
  exact_optional_property_types: Option<bool>,
  #[serde(default)]
  no_unchecked_indexed_access: Option<bool>,
  #[serde(default)]
  no_lib: Option<bool>,
  #[serde(default)]
  no_default_lib: Option<bool>,
  #[serde(default)]
  use_define_for_class_fields: Option<bool>,
  #[serde(default)]
  jsx: Option<String>,
  #[serde(default)]
  jsx_import_source: Option<String>,
  #[serde(default)]
  base_url: Option<String>,
  #[serde(default)]
  paths: Option<BTreeMap<String, Vec<String>>>,
}

pub fn load_project_config(project: &Path) -> Result<ProjectConfig, String> {
  let tsconfig_path = resolve_tsconfig_path(project)?;
  let root_dir = tsconfig_path
    .parent()
    .ok_or_else(|| format!("invalid tsconfig path {}", tsconfig_path.display()))?
    .to_path_buf();
  let mut visited = HashSet::new();
  let raw = load_raw_config(&tsconfig_path, &root_dir, &mut visited)?;

  let compiler_options = compiler_options_from_raw(&raw.compiler_options)?;
  let mut base_url = raw
    .compiler_options
    .base_url
    .as_deref()
    .map(|raw| resolve_path_relative_to(&root_dir, Path::new(raw)));
  let paths = raw.compiler_options.paths.clone().unwrap_or_default();
  if base_url.is_none() && !paths.is_empty() {
    base_url = Some(root_dir.clone());
  }

  let root_files = discover_root_files(&root_dir, &raw)?;

  let types = raw
    .compiler_options
    .types
    .as_ref()
    .map(|types| normalize_string_list(types));
  let type_roots = raw.compiler_options.type_roots.as_ref().map(|roots| {
    normalize_string_list(roots)
      .into_iter()
      .map(|raw| resolve_path_relative_to(&root_dir, Path::new(&raw)))
      .collect()
  });
  let jsx_import_source = raw
    .compiler_options
    .jsx_import_source
    .as_deref()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());

  Ok(ProjectConfig {
    root_dir,
    compiler_options,
    base_url,
    paths,
    root_files,
    types,
    type_roots,
    jsx_import_source,
  })
}

fn resolve_tsconfig_path(project: &Path) -> Result<PathBuf, String> {
  let candidate = if project.is_dir() {
    project.join("tsconfig.json")
  } else {
    project.to_path_buf()
  };
  let absolute = if candidate.is_absolute() {
    candidate
  } else {
    std::env::current_dir()
      .map_err(|err| format!("failed to resolve current directory: {err}"))?
      .join(candidate)
  };
  absolute
    .canonicalize()
    .map_err(|err| format!("failed to read tsconfig {}: {err}", absolute.display()))
}

fn load_raw_config(
  path: &Path,
  root_dir: &Path,
  visited: &mut HashSet<PathBuf>,
) -> Result<RawTsConfig, String> {
  let canonical = path
    .canonicalize()
    .map_err(|err| format!("failed to read tsconfig {}: {err}", path.display()))?;
  if !visited.insert(canonical.clone()) {
    return Err(format!(
      "cycle detected while resolving tsconfig extends: {}",
      canonical.display()
    ));
  }

  let text = fs::read_to_string(&canonical)
    .map_err(|err| format!("failed to read {}: {err}", canonical.display()))?;
  let mut current: RawTsConfig =
    json5::from_str(&text).map_err(|err| format!("failed to parse {}: {err}", canonical.display()))?;
  let config_dir = canonical
    .parent()
    .ok_or_else(|| format!("invalid tsconfig path {}", canonical.display()))?;
  resolve_raw_config_paths(&mut current, config_dir, root_dir);

  let Some(extends) = current.extends.take() else {
    return Ok(current);
  };

  let extends_path = resolve_extends_path(config_dir, &extends)?;
  let base = load_raw_config(&extends_path, root_dir, visited)?;
  Ok(merge_raw_configs(base, current))
}

fn resolve_raw_config_paths(config: &mut RawTsConfig, config_dir: &Path, root_dir: &Path) {
  if let Some(files) = config.files.as_mut() {
    for file in files.iter_mut() {
      *file = resolve_path_string_relative_to(config_dir, file);
    }
  }
  if let Some(include) = config.include.as_mut() {
    for pattern in include.iter_mut() {
      *pattern = rewrite_glob_pattern(config_dir, root_dir, pattern);
    }
  }
  if let Some(exclude) = config.exclude.as_mut() {
    for pattern in exclude.iter_mut() {
      *pattern = rewrite_glob_pattern(config_dir, root_dir, pattern);
    }
  }
  if let Some(base_url) = config.compiler_options.base_url.as_mut() {
    *base_url = resolve_path_string_relative_to(config_dir, base_url);
  }
  if let Some(type_roots) = config.compiler_options.type_roots.as_mut() {
    for root in type_roots.iter_mut() {
      *root = resolve_path_string_relative_to(config_dir, root);
    }
  }
}

fn resolve_extends_path(config_dir: &Path, extends: &str) -> Result<PathBuf, String> {
  let extends = extends.trim();
  if extends.is_empty() {
    return Err("tsconfig `extends` value is empty".into());
  }
  let mut candidate = if Path::new(extends).is_absolute() {
    PathBuf::from(extends)
  } else {
    config_dir.join(extends)
  };
  if candidate.is_dir() {
    candidate = candidate.join("tsconfig.json");
  } else if candidate.extension().is_none() {
    candidate.set_extension("json");
  }
  candidate
    .canonicalize()
    .map_err(|err| format!("failed to read tsconfig extends {}: {err}", candidate.display()))
}

fn merge_raw_configs(mut base: RawTsConfig, next: RawTsConfig) -> RawTsConfig {
  merge_compiler_options(&mut base.compiler_options, next.compiler_options);
  // `files` overrides and does not merge.
  if next.files.is_some() {
    base.files = next.files;
  }
  // `include` merges when present.
  if let Some(include) = next.include {
    base.include.get_or_insert_with(Vec::new).extend(include);
  }
  // `exclude` merges when present.
  if let Some(exclude) = next.exclude {
    base.exclude.get_or_insert_with(Vec::new).extend(exclude);
  }
  base
}

fn merge_compiler_options(base: &mut RawCompilerOptions, next: RawCompilerOptions) {
  if next.target.is_some() {
    base.target = next.target;
  }
  if next.module.is_some() {
    base.module = next.module;
  }
  if next.lib.is_some() {
    base.lib = next.lib;
  }
  if next.types.is_some() {
    base.types = next.types;
  }
  if next.type_roots.is_some() {
    base.type_roots = next.type_roots;
  }
  if next.module_resolution.is_some() {
    base.module_resolution = next.module_resolution;
  }
  if next.skip_lib_check.is_some() {
    base.skip_lib_check = next.skip_lib_check;
  }
  if next.no_emit.is_some() {
    base.no_emit = next.no_emit;
  }
  if next.no_emit_on_error.is_some() {
    base.no_emit_on_error = next.no_emit_on_error;
  }
  if next.declaration.is_some() {
    base.declaration = next.declaration;
  }
  if next.strict.is_some() {
    base.strict = next.strict;
  }
  if next.no_implicit_any.is_some() {
    base.no_implicit_any = next.no_implicit_any;
  }
  if next.strict_null_checks.is_some() {
    base.strict_null_checks = next.strict_null_checks;
  }
  if next.strict_function_types.is_some() {
    base.strict_function_types = next.strict_function_types;
  }
  if next.exact_optional_property_types.is_some() {
    base.exact_optional_property_types = next.exact_optional_property_types;
  }
  if next.no_unchecked_indexed_access.is_some() {
    base.no_unchecked_indexed_access = next.no_unchecked_indexed_access;
  }
  if next.no_lib.is_some() {
    base.no_lib = next.no_lib;
  }
  if next.no_default_lib.is_some() {
    base.no_default_lib = next.no_default_lib;
  }
  if next.use_define_for_class_fields.is_some() {
    base.use_define_for_class_fields = next.use_define_for_class_fields;
  }
  if next.jsx.is_some() {
    base.jsx = next.jsx;
  }
  if next.jsx_import_source.is_some() {
    base.jsx_import_source = next.jsx_import_source;
  }
  if next.base_url.is_some() {
    base.base_url = next.base_url;
  }
  if next.paths.is_some() {
    base.paths = next.paths;
  }
}

fn discover_root_files(root_dir: &Path, config: &RawTsConfig) -> Result<Vec<PathBuf>, String> {
  if let Some(files) = config.files.as_ref() {
    let files = normalize_string_list(files);
    let mut out: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(f)).collect();
    out.sort_by(|a, b| a.display().to_string().cmp(&b.display().to_string()));
    out.dedup();
    return Ok(out);
  }

  let include = normalize_string_list(config.include.as_deref().unwrap_or(&[]));
  let exclude = normalize_string_list(config.exclude.as_deref().unwrap_or(&[]));
  let include_set = build_globset(root_dir, &include)?;
  let exclude_set = build_globset(root_dir, &exclude)?;
  let mut files = Vec::new();
  for entry in WalkDir::new(root_dir)
    .follow_links(true)
    .into_iter()
    .filter_map(|e| e.ok())
  {
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();
    if !is_ts_like_file(path) {
      continue;
    }
    let rel = path.strip_prefix(root_dir).unwrap_or(path);
    if !include.is_empty() && !include_set.is_match(rel) {
      continue;
    }
    if exclude_set.is_match(rel) {
      continue;
    }
    files.push(path.to_path_buf());
  }
  files.sort_by(|a, b| a.display().to_string().cmp(&b.display().to_string()));
  files.dedup();
  Ok(files)
}

fn is_ts_like_file(path: &Path) -> bool {
  let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
  let name = name.to_ascii_lowercase();
  if name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts") {
    return false;
  }
  name.ends_with(".ts")
    || name.ends_with(".tsx")
    || name.ends_with(".mts")
    || name.ends_with(".cts")
    || name.ends_with(".js")
    || name.ends_with(".jsx")
    || name.ends_with(".mjs")
    || name.ends_with(".cjs")
}

fn build_globset(root_dir: &Path, patterns: &[String]) -> Result<GlobSet, String> {
  let mut builder = GlobSetBuilder::new();
  for pattern in patterns {
    let pattern = pattern.trim();
    if pattern.is_empty() {
      continue;
    }
    let glob = Glob::new(pattern)
      .map_err(|err| format!("invalid glob pattern '{pattern}' in {}: {err}", root_dir.display()))?;
    builder.add(glob);
  }
  builder
    .build()
    .map_err(|err| format!("failed to compile globs for {}: {err}", root_dir.display()))
}

fn rewrite_glob_pattern(config_dir: &Path, root_dir: &Path, pattern: &str) -> String {
  let pattern = pattern.trim();
  if pattern.is_empty() {
    return pattern.to_string();
  }
  // TS patterns are resolved relative to the config file, but downstream glob
  // matching is relative to the project root. Rewrite the prefix.
  let path = resolve_path_relative_to(config_dir, Path::new(pattern));
  let rel = path.strip_prefix(root_dir).unwrap_or(&path);
  rel.to_string_lossy().to_string()
}

fn resolve_path_string_relative_to(base: &Path, raw: &str) -> String {
  resolve_path_relative_to(base, Path::new(raw))
    .to_string_lossy()
    .to_string()
}

fn resolve_path_relative_to(base: &Path, raw: &Path) -> PathBuf {
  if raw.is_absolute() {
    raw.to_path_buf()
  } else {
    base.join(raw)
  }
}

fn normalize_string_list(values: &[String]) -> Vec<String> {
  let mut out: Vec<String> = values
    .iter()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect();
  out.sort();
  out.dedup();
  out
}

fn compiler_options_from_raw(raw: &RawCompilerOptions) -> Result<CompilerOptions, String> {
  let mut options = CompilerOptions::default();

  if let Some(target) = raw.target.as_deref() {
    options.target = parse_script_target(target).ok_or_else(|| {
      format!(
        "unsupported compilerOptions.target '{target}' (supported: ES3, ES5, ES2015, ES2016, ES2017, ES2018, ES2019, ES2020, ES2021, ES2022, ESNext)"
      )
    })?;
  }
  if let Some(module) = raw.module.as_deref() {
    options.module = Some(parse_module_kind(module).ok_or_else(|| {
      format!(
        "unsupported compilerOptions.module '{module}' (supported: None, CommonJS, ES2015, ES2020, ES2022, ESNext, UMD, AMD, System, Node16, NodeNext)"
      )
    })?);
  }
  if let Some(libs) = raw.lib.as_ref() {
    options.libs = libs
      .iter()
      .filter_map(|s| LibName::parse(s))
      .collect::<Vec<_>>();
  }
  if let Some(types) = raw.types.as_ref() {
    options.types = normalize_string_list(types);
  }
  options.module_resolution = raw
    .module_resolution
    .as_deref()
    .map(|s| s.trim().to_ascii_lowercase())
    .filter(|s| !s.is_empty());
  if let Some(skip) = raw.skip_lib_check {
    options.skip_lib_check = skip;
  }
  if let Some(no_emit) = raw.no_emit {
    options.no_emit = no_emit;
  }
  if let Some(no_emit_on_error) = raw.no_emit_on_error {
    options.no_emit_on_error = no_emit_on_error;
  }
  if let Some(decl) = raw.declaration {
    options.declaration = decl;
  }
  if let Some(strict) = raw.strict {
    if strict {
      options.strict_null_checks = true;
      options.no_implicit_any = false;
      options.strict_function_types = true;
      options.exact_optional_property_types = false;
      options.no_unchecked_indexed_access = false;
    } else {
      options.strict_null_checks = false;
      options.no_implicit_any = false;
      options.strict_function_types = false;
    }
  }
  if let Some(no_any) = raw.no_implicit_any {
    options.no_implicit_any = no_any;
  }
  if let Some(strict_null) = raw.strict_null_checks {
    options.strict_null_checks = strict_null;
  }
  if let Some(strict_fn) = raw.strict_function_types {
    options.strict_function_types = strict_fn;
  }
  if let Some(exact) = raw.exact_optional_property_types {
    options.exact_optional_property_types = exact;
  }
  if let Some(no_unchecked) = raw.no_unchecked_indexed_access {
    options.no_unchecked_indexed_access = no_unchecked;
  }
  if let Some(no_lib) = raw.no_lib {
    options.no_default_lib = no_lib;
  }
  if let Some(no_default_lib) = raw.no_default_lib {
    options.no_default_lib = no_default_lib;
  }
  if let Some(define) = raw.use_define_for_class_fields {
    options.use_define_for_class_fields = define;
  }
  options.jsx = raw.jsx.as_deref().and_then(parse_jsx_mode);

  Ok(options)
}

fn parse_script_target(raw: &str) -> Option<ScriptTarget> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "es3" => Some(ScriptTarget::Es3),
    "es5" => Some(ScriptTarget::Es5),
    "es2015" | "es6" => Some(ScriptTarget::Es2015),
    "es2016" => Some(ScriptTarget::Es2016),
    "es2017" => Some(ScriptTarget::Es2017),
    "es2018" => Some(ScriptTarget::Es2018),
    "es2019" => Some(ScriptTarget::Es2019),
    "es2020" => Some(ScriptTarget::Es2020),
    "es2021" => Some(ScriptTarget::Es2021),
    "es2022" => Some(ScriptTarget::Es2022),
    "esnext" => Some(ScriptTarget::EsNext),
    _ => None,
  }
}

fn parse_module_kind(raw: &str) -> Option<ModuleKind> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "none" => Some(ModuleKind::None),
    "commonjs" => Some(ModuleKind::CommonJs),
    "es2015" | "es6" => Some(ModuleKind::Es2015),
    "es2020" => Some(ModuleKind::Es2020),
    "es2022" => Some(ModuleKind::Es2022),
    "esnext" => Some(ModuleKind::EsNext),
    "umd" => Some(ModuleKind::Umd),
    "amd" => Some(ModuleKind::Amd),
    "system" => Some(ModuleKind::System),
    "node16" => Some(ModuleKind::Node16),
    "nodenext" => Some(ModuleKind::NodeNext),
    _ => None,
  }
}

fn parse_jsx_mode(raw: &str) -> Option<JsxMode> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "preserve" => Some(JsxMode::Preserve),
    "react" => Some(JsxMode::React),
    "react-jsx" => Some(JsxMode::ReactJsx),
    "react-jsxdev" => Some(JsxMode::ReactJsxdev),
    _ => None,
  }
}

