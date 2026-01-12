#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

use diagnostics::{Diagnostic, FileId, Span, TextRange};
use types_ts_interned::CacheConfig;

#[cfg(feature = "serde")]
fn is_false(value: &bool) -> bool {
  !*value
}

/// Target language level.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScriptTarget {
  Es3,
  Es5,
  Es2015,
  Es2016,
  Es2017,
  Es2018,
  Es2019,
  Es2020,
  Es2021,
  Es2022,
  EsNext,
}

/// JSX transform mode.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JsxMode {
  Preserve,
  React,
  ReactJsx,
  ReactJsxdev,
}

/// Module system to emit/parse.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleKind {
  None,
  CommonJs,
  Es2015,
  Es2020,
  Es2022,
  EsNext,
  Umd,
  Amd,
  System,
  Node16,
  NodeNext,
}

impl ModuleKind {
  pub fn option_name(&self) -> &'static str {
    match self {
      ModuleKind::None => "None",
      ModuleKind::CommonJs => "CommonJS",
      ModuleKind::Es2015 => "ES2015",
      ModuleKind::Es2020 => "ES2020",
      ModuleKind::Es2022 => "ES2022",
      ModuleKind::EsNext => "ESNext",
      ModuleKind::Umd => "UMD",
      ModuleKind::Amd => "AMD",
      ModuleKind::System => "System",
      ModuleKind::Node16 => "Node16",
      ModuleKind::NodeNext => "NodeNext",
    }
  }
}

/// TypeScript `moduleResolution` mode that influences module specifier
/// resolution.
///
/// This models the relevant subset of TypeScript's `ModuleResolutionKind` enum.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleResolutionKind {
  Classic,
  Node10,
  Node16,
  NodeNext,
  Bundler,
}

impl ModuleResolutionKind {
  /// Canonical string form accepted by `tsc` (`--moduleResolution`).
  pub fn as_str(&self) -> &'static str {
    match self {
      ModuleResolutionKind::Classic => "classic",
      // Prefer `node` rather than `node10` to avoid triggering TypeScript's
      // deprecation warning for the `node10` spelling when explicitly set.
      ModuleResolutionKind::Node10 => "node",
      ModuleResolutionKind::Node16 => "node16",
      ModuleResolutionKind::NodeNext => "nodenext",
      ModuleResolutionKind::Bundler => "bundler",
    }
  }

  /// Name used by TypeScript diagnostics (`ModuleResolutionKind[moduleResolution]`).
  pub fn option_name(&self) -> &'static str {
    match self {
      ModuleResolutionKind::Classic => "Classic",
      ModuleResolutionKind::Node10 => "Node10",
      ModuleResolutionKind::Node16 => "Node16",
      ModuleResolutionKind::NodeNext => "NodeNext",
      ModuleResolutionKind::Bundler => "Bundler",
    }
  }
}

/// TypeScript `moduleDetection` mode (`--moduleDetection`).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleDetectionKind {
  Auto,
  Force,
  Legacy,
}

impl ModuleDetectionKind {
  pub fn as_str(&self) -> &'static str {
    match self {
      ModuleDetectionKind::Auto => "auto",
      ModuleDetectionKind::Force => "force",
      ModuleDetectionKind::Legacy => "legacy",
    }
  }
}

/// Compute the effective TypeScript `module` setting, applying `tsc`'s defaults
/// when the option is unset.
///
/// `tsc` defaults `module` based on `target`:
/// - ES3/ES5 -> CommonJS
/// - otherwise -> ES2015
pub fn effective_module_kind(options: &CompilerOptions) -> ModuleKind {
  options.effective_module_kind()
}

impl Default for ScriptTarget {
  fn default() -> Self {
    ScriptTarget::Es2015
  }
}

#[cfg(feature = "resolve")]
pub fn effective_module_resolution_mode(options: &CompilerOptions) -> crate::resolve::ModuleResolutionMode {
  use crate::resolve::ModuleResolutionMode;
  match options.effective_module_resolution() {
    ModuleResolutionKind::Classic => ModuleResolutionMode::Classic,
    ModuleResolutionKind::Node10 => ModuleResolutionMode::Node10,
    ModuleResolutionKind::Node16 => ModuleResolutionMode::Node16,
    ModuleResolutionKind::NodeNext => ModuleResolutionMode::NodeNext,
    ModuleResolutionKind::Bundler => ModuleResolutionMode::Bundler,
  }
}

/// Compute default resolver settings for [`crate::resolve::NodeResolver`].
///
/// This helper centralizes `tsc`-compatible defaulting for:
/// - `module` (via [`effective_module_kind`])
/// - `moduleResolution` (via [`effective_module_resolution_mode`])
/// - `node_modules`/package-imports flags derived from `moduleResolution`
#[cfg(feature = "resolve")]
pub fn resolve_options_for_node_resolver(
  options: &CompilerOptions,
) -> crate::resolve::ResolveOptions {
  options.effective_resolve_options()
}

/// Compiler configuration that materially affects lib selection and typing.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompilerOptions {
  pub target: ScriptTarget,
  pub module: Option<ModuleKind>,
  /// If true, do not automatically include default libs.
  pub no_default_lib: bool,
  /// Explicit lib overrides (when non-empty this replaces the default target-derived set).
  pub libs: Vec<LibName>,
  /// Whether to skip checking bundled and host-provided libs.
  pub skip_lib_check: bool,
  /// Whether to suppress emit; the checker never emits today, but we keep the flag for parity.
  pub no_emit: bool,
  /// Whether to suppress emit on error; unused for now but tracked for fidelity.
  pub no_emit_on_error: bool,
  /// Whether to produce declaration outputs; unused in the checker but surfaced for completeness.
  pub declaration: bool,
  /// Module resolution strategy as provided by the host (raw, lower-cased string).
  pub module_resolution: Option<String>,
  /// Explicitly included `@types` packages.
  pub types: Vec<String>,
  /// Allow JavaScript files to be part of the program (`allowJs` / `compilerOptions.allowJs`).
  ///
  /// The checker currently accepts JS roots unconditionally, but we keep this flag
  /// so harnesses can round-trip TypeScript test directives and future work can
  /// gate JS-specific semantics as needed.
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub allow_js: bool,
  /// Enable full checking of JavaScript files (`checkJs` / `compilerOptions.checkJs`).
  ///
  /// This is tracked for parity with `tsc` and may be used by downstream tools
  /// to suppress diagnostics in JS sources when disabled.
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub check_js: bool,
  /// Module detection strategy (`moduleDetection` / `compilerOptions.moduleDetection`).
  ///
  /// Stored as the raw, lower-cased string to match the harness/tsconfig parsing
  /// model used elsewhere (e.g. `moduleResolution`).
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub module_detection: Option<String>,
  /// JSX import source package (`jsxImportSource` / `compilerOptions.jsxImportSource`).
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  /// Base module used for automatic JSX runtime imports.
  ///
  /// When `jsx` is set to `react-jsx`/`react-jsxdev`, TypeScript implicitly
  /// imports either `{jsxImportSource}/jsx-runtime` or
  /// `{jsxImportSource}/jsx-dev-runtime` for JSX helper functions and associated
  /// `JSX.*` namespace typings.
  pub jsx_import_source: Option<String>,
  pub strict_null_checks: bool,
  pub no_implicit_any: bool,
  /// Enforce the AOT-friendly subset of TypeScript described in EXEC.plan.
  ///
  /// Native-strict relies on sound nullability and therefore requires
  /// [`strict_null_checks`](Self::strict_null_checks) to be enabled.
  ///
  /// This is intentionally opt-in so existing conformance behavior remains
  /// unchanged unless explicitly enabled.
  #[cfg_attr(feature = "serde", serde(default))]
  pub native_strict: bool,
  pub strict_function_types: bool,
  pub exact_optional_property_types: bool,
  pub no_unchecked_indexed_access: bool,
  /// Legacy alias for [`native_strict`](Self::native_strict).
  ///
  /// This is a repo-specific dialect that forbids dynamic JavaScript constructs
  /// (e.g. `eval`, `with`, `Proxy`, prototype mutation) that break
  /// ahead-of-time optimizations and soundness.
  ///
  /// Older tooling used the `strict_native` option name. Keep it as a separate
  /// flag for compatibility, but new integrations should prefer
  /// [`native_strict`](Self::native_strict).
  #[cfg_attr(feature = "serde", serde(default))]
  pub strict_native: bool,
  /// Whether class fields follow ECMAScript `define` semantics (`Object.defineProperty`)
  /// or legacy assignment semantics.
  ///
  /// The checker uses this option when diagnosing `this.x` reads inside class
  /// field initializers:
  /// - When targeting native class fields (ES2022/ESNext) and `useDefineForClassFields`
  ///   is enabled, reading a constructor parameter property (e.g.
  ///   `constructor(public x: number)`) from a field initializer reports `TS2729`.
  /// - When `useDefineForClassFields` is disabled (assignment semantics),
  ///   `TS2729` is suppressed if the property exists on a base class, matching
  ///   `tsc`'s behavior.
  pub use_define_for_class_fields: bool,
  pub jsx: Option<JsxMode>,
  /// Cache sizing and sharing strategy for the checker.
  pub cache: CacheOptions,
}

impl Default for CompilerOptions {
  fn default() -> Self {
    CompilerOptions {
      target: ScriptTarget::default(),
      module: None,
      no_default_lib: false,
      libs: Vec::new(),
      skip_lib_check: true,
      no_emit: true,
      no_emit_on_error: false,
      declaration: false,
      module_resolution: None,
      types: Vec::new(),
      allow_js: false,
      check_js: false,
      module_detection: None,
      jsx_import_source: None,
      strict_null_checks: true,
      no_implicit_any: false,
      native_strict: false,
      strict_function_types: true,
      exact_optional_property_types: false,
      no_unchecked_indexed_access: false,
      strict_native: false,
      use_define_for_class_fields: true,
      jsx: None,
      cache: CacheOptions::default(),
    }
  }
}

impl CompilerOptions {
  /// Canonicalize option values so that downstream behavior is deterministic
  /// regardless of how the options were specified (ordering, casing, etc).
  ///
  /// This does **not** emit diagnostics. Use [`Self::normalize_and_validate`] to
  /// apply `tsc`-style option validation that also produces diagnostics.
  pub fn normalize(mut self) -> Self {
    // `strict_native` is a legacy alias for `native_strict`. Treat them as fully
    // synonymous even when only one is explicitly set by the host API.
    let native_strict = self.native_strict || self.strict_native;
    self.native_strict = native_strict;
    self.strict_native = native_strict;

    self.module_resolution = self
      .module_resolution
      .take()
      .and_then(|raw| {
        normalize_optional_string(raw, |s| {
          canonicalize_module_resolution_value(&s.to_ascii_lowercase())
            .unwrap_or_else(|| s.to_ascii_lowercase())
        })
      });

    self.module_detection = self
      .module_detection
      .take()
      .and_then(|raw| normalize_optional_string(raw, |s| s.to_ascii_lowercase()));
    self.jsx_import_source = self
      .jsx_import_source
      .take()
      .and_then(|raw| normalize_optional_string(raw, |s| s.to_string()));

    if self.libs.len() > 1 {
      self.libs.sort();
      self.libs.dedup();
    }

    if !self.types.is_empty() {
      let mut normalized: Vec<String> = self
        .types
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
      normalized.sort();
      normalized.dedup();
      self.types = normalized;
    }

    self
  }

  /// Normalize and validate compiler options, returning the updated options and
  /// any diagnostics produced during validation.
  ///
  /// Validation may apply `tsc`-compatible fixups (for example, a conflicting
  /// `noLib` + `lib` combination emits `TS5053` and then ignores the `lib` list).
  pub fn normalize_and_validate(self) -> (Self, Vec<Diagnostic>) {
    let mut options = self.normalize();
    let diagnostics = validate_options(&mut options);
    (options, diagnostics)
  }

  /// Compute the effective `module` kind following TypeScript's computed
  /// defaults (`getEmitModuleKind`).
  ///
  /// When `module` is unset, TypeScript defaults it based on `target`:
  /// - `target >= ES2015` => `ES2015`
  /// - otherwise => `CommonJS`
  pub fn effective_module_kind(&self) -> ModuleKind {
    self.module.unwrap_or_else(|| {
      if self.target.is_es2015_or_later() {
        ModuleKind::Es2015
      } else {
        ModuleKind::CommonJs
      }
    })
  }

  /// Compute the effective `moduleResolution` following TypeScript's computed
  /// defaults (`getEmitModuleResolutionKind`).
  pub fn effective_module_resolution(&self) -> ModuleResolutionKind {
    if let Some(raw) = self.module_resolution.as_deref() {
      if let Some(kind) = parse_module_resolution_kind(raw) {
        return kind;
      }
    }

    let module_kind = self.effective_module_kind();
    match module_kind {
      ModuleKind::None | ModuleKind::Amd | ModuleKind::Umd | ModuleKind::System => {
        ModuleResolutionKind::Classic
      }
      ModuleKind::NodeNext => ModuleResolutionKind::NodeNext,
      ModuleKind::Node16 => ModuleResolutionKind::Node16,
      _ => ModuleResolutionKind::Bundler,
    }
  }

  /// Compute the effective `moduleDetection` following TypeScript's computed
  /// defaults (`getEmitModuleDetectionKind`).
  pub fn effective_module_detection(&self) -> ModuleDetectionKind {
    if let Some(raw) = self.module_detection.as_deref() {
      if let Some(kind) = parse_module_detection_kind(raw) {
        return kind;
      }
    }

    match self.effective_module_kind() {
      ModuleKind::Node16 | ModuleKind::NodeNext => ModuleDetectionKind::Force,
      _ => ModuleDetectionKind::Auto,
    }
  }

  /// Translate the effective compiler options into resolver settings for
  /// `typecheck_ts::resolve`.
  #[cfg(feature = "resolve")]
  pub fn effective_resolve_options(&self) -> crate::resolve::ResolveOptions {
    use crate::resolve::{ModuleResolutionMode, ResolveOptions};
    let module_kind = self.effective_module_kind();
    match self.effective_module_resolution() {
      ModuleResolutionKind::Classic => ResolveOptions {
        node_modules: false,
        package_imports: false,
        module_resolution: ModuleResolutionMode::Classic,
        module_kind: Some(module_kind),
        ..ResolveOptions::default()
      },
      ModuleResolutionKind::Node10 => ResolveOptions {
        node_modules: true,
        package_imports: false,
        module_resolution: ModuleResolutionMode::Node10,
        module_kind: Some(module_kind),
        ..ResolveOptions::default()
      },
      ModuleResolutionKind::Node16 => ResolveOptions {
        node_modules: true,
        package_imports: true,
        module_resolution: ModuleResolutionMode::Node16,
        module_kind: Some(module_kind),
        ..ResolveOptions::default()
      },
      ModuleResolutionKind::NodeNext => ResolveOptions {
        node_modules: true,
        package_imports: true,
        module_resolution: ModuleResolutionMode::NodeNext,
        module_kind: Some(module_kind),
        ..ResolveOptions::default()
      },
      ModuleResolutionKind::Bundler => ResolveOptions {
        node_modules: true,
        package_imports: true,
        module_resolution: ModuleResolutionMode::Bundler,
        module_kind: Some(module_kind),
        ..ResolveOptions::default()
      },
    }
  }
}

/// Strategy for sharing caches across bodies/files.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CacheMode {
  /// Reuse the same caches across bodies for maximal hit rates.
  Shared,
  /// Create fresh caches for each body and drop them afterwards.
  PerBody,
}

/// Cache sizing controls exposed through the host.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheOptions {
  pub max_relation_cache_entries: usize,
  pub max_eval_cache_entries: usize,
  pub max_instantiation_cache_entries: usize,
  pub max_body_cache_entries: usize,
  pub max_def_cache_entries: usize,
  pub cache_shards: usize,
  pub mode: CacheMode,
}

impl CacheOptions {
  pub fn relation_config(&self) -> CacheConfig {
    CacheConfig {
      max_entries: self.max_relation_cache_entries,
      shard_count: self.cache_shards,
    }
  }

  pub fn eval_config(&self) -> CacheConfig {
    CacheConfig {
      max_entries: self.max_eval_cache_entries,
      shard_count: self.cache_shards,
    }
  }

  pub fn instantiation_config(&self) -> CacheConfig {
    CacheConfig {
      max_entries: self.max_instantiation_cache_entries,
      shard_count: self.cache_shards,
    }
  }

  pub fn body_config(&self) -> CacheConfig {
    CacheConfig {
      max_entries: self.max_body_cache_entries,
      shard_count: self.cache_shards,
    }
  }

  pub fn def_config(&self) -> CacheConfig {
    CacheConfig {
      max_entries: self.max_def_cache_entries,
      shard_count: self.cache_shards,
    }
  }
}

impl Default for CacheOptions {
  fn default() -> Self {
    Self {
      max_relation_cache_entries: CacheConfig::default().max_entries,
      max_eval_cache_entries: CacheConfig::default().max_entries,
      max_instantiation_cache_entries: CacheConfig::default().max_entries / 2,
      max_body_cache_entries: CacheConfig::default().max_entries,
      max_def_cache_entries: CacheConfig::default().max_entries,
      cache_shards: CacheConfig::default().shard_count,
      mode: CacheMode::Shared,
    }
  }
}

/// Named libraries that can be loaded.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct LibName(Arc<str>);

impl LibName {
  /// Parse a TypeScript lib name from the `--lib` / `compilerOptions.lib` model.
  ///
  /// Accepts the canonical names (e.g. `es2020`, `dom.iterable`), common case
  /// variants (e.g. `ES2020`), and the filename form (e.g. `lib.es2020.d.ts`).
  ///
  /// Returns `None` when the string cannot represent a TS lib name.
  pub fn parse(raw: &str) -> Option<Self> {
    canonicalize_lib_name(raw).map(|name| LibName(Arc::from(name)))
  }

  /// Parse a lib name from a `--lib` / `compilerOptions.lib` entry, preserving
  /// values that are invalid so the bundled lib loader can emit `TS6046`.
  ///
  /// This is intentionally more permissive than [`Self::parse`]. It performs
  /// best-effort canonicalization (trim, lower-case, strip `lib.` prefix and
  /// `.d.ts`/`.ts` suffix) but does not require the resulting string to match
  /// the set of known TypeScript libs.
  pub fn from_compiler_option_value(raw: &str) -> Option<Self> {
    normalize_lib_option_value(raw).map(|name| LibName(Arc::from(name)))
  }

  /// Parse a lib name from TypeScript-style option strings (e.g. `dom`, `es2020`,
  /// `esnext.disposable`). This is a small compatibility shim used by features
  /// like `/// <reference lib="..." />`.
  pub fn from_option_name(raw: &str) -> Option<Self> {
    LibName::parse(raw)
  }

  /// Canonical lib name used by TypeScript (lower-cased, no `lib.` / `.d.ts`).
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Filename used by the TypeScript lib bundle (`lib.<name>.d.ts`).
  pub fn file_name(&self) -> String {
    format!("lib.{}.d.ts", self.as_str())
  }
}

impl fmt::Display for LibName {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

fn normalize_optional_string(
  raw: String,
  map: impl FnOnce(&str) -> String,
) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  Some(map(trimmed))
}

fn validate_options(options: &mut CompilerOptions) -> Vec<Diagnostic> {
  use crate::codes;

  let mut diagnostics = Vec::new();
  let placeholder = Span::new(FileId(u32::MAX), TextRange::new(0, 0));

  // Match tsc's TS5053 behaviour: `--noLib` conflicts with an explicit `--lib`
  // list. TypeScript emits the diagnostic and then proceeds as `--noLib`
  // (ignoring the `lib` list entirely).
  if options.no_default_lib && !options.libs.is_empty() {
    diagnostics.push(codes::LIB_OPTION_CANNOT_BE_SPECIFIED_WITH_NOLIB.error(
      "Option 'lib' cannot be specified with option 'noLib'.",
      placeholder,
    ));
    options.libs.clear();
  }

  if let Some(raw) = options.module_resolution.as_deref() {
    if parse_module_resolution_kind(raw).is_none() {
      diagnostics.push(codes::INVALID_OPTION_VALUE.error(
        "Argument for '--moduleResolution' option must be: 'classic', 'node', 'node16', 'nodenext', 'bundler'.",
        placeholder,
      ));
      // Treat invalid values as if the option was unset.
      options.module_resolution = None;
    } else if let Some(canonical) = canonicalize_module_resolution_value(raw) {
      if canonical != raw {
        options.module_resolution = Some(canonical);
      }
    }
  }

  let module_kind = options.effective_module_kind();
  if let Some(module_resolution) = options
    .module_resolution
    .as_deref()
    .and_then(parse_module_resolution_kind)
  {
    if matches!(
      module_resolution,
      ModuleResolutionKind::Node16 | ModuleResolutionKind::NodeNext
    ) && !matches!(module_kind, ModuleKind::Node16 | ModuleKind::NodeNext)
    {
      let name = module_resolution.option_name();
      diagnostics.push(codes::MODULE_OPTION_REQUIRED_FOR_NODE16_RESOLUTION.error(
        format!("Option 'module' must be set to '{name}' when option 'moduleResolution' is set to '{name}'."),
        placeholder,
      ));
    }
  }

  diagnostics
}

impl ScriptTarget {
  fn is_es2015_or_later(&self) -> bool {
    matches!(
      self,
      ScriptTarget::Es2015
        | ScriptTarget::Es2016
        | ScriptTarget::Es2017
        | ScriptTarget::Es2018
        | ScriptTarget::Es2019
        | ScriptTarget::Es2020
        | ScriptTarget::Es2021
        | ScriptTarget::Es2022
        | ScriptTarget::EsNext
    )
  }
}

/// Parse a TypeScript `target` option value.
pub fn parse_script_target(raw: &str) -> Option<ScriptTarget> {
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

/// Parse a TypeScript `module` option value.
pub fn parse_module_kind(raw: &str) -> Option<ModuleKind> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "none" => Some(ModuleKind::None),
    "commonjs" => Some(ModuleKind::CommonJs),
    "amd" => Some(ModuleKind::Amd),
    "umd" => Some(ModuleKind::Umd),
    "system" => Some(ModuleKind::System),
    "es2015" | "es6" => Some(ModuleKind::Es2015),
    "es2020" => Some(ModuleKind::Es2020),
    "es2022" => Some(ModuleKind::Es2022),
    "esnext" => Some(ModuleKind::EsNext),
    "node16" => Some(ModuleKind::Node16),
    "nodenext" => Some(ModuleKind::NodeNext),
    _ => None,
  }
}

/// Parse a TypeScript `jsx` option value.
pub fn parse_jsx_mode(raw: &str) -> Option<JsxMode> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "preserve" | "react-native" => Some(JsxMode::Preserve),
    "react" => Some(JsxMode::React),
    "react-jsx" => Some(JsxMode::ReactJsx),
    "react-jsxdev" => Some(JsxMode::ReactJsxdev),
    _ => None,
  }
}

pub fn parse_module_resolution_kind(raw: &str) -> Option<ModuleResolutionKind> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "classic" => Some(ModuleResolutionKind::Classic),
    "node" | "nodejs" | "node10" => Some(ModuleResolutionKind::Node10),
    "node16" => Some(ModuleResolutionKind::Node16),
    "nodenext" => Some(ModuleResolutionKind::NodeNext),
    "bundler" => Some(ModuleResolutionKind::Bundler),
    _ => None,
  }
}

pub fn parse_module_detection_kind(raw: &str) -> Option<ModuleDetectionKind> {
  match raw.trim().to_ascii_lowercase().as_str() {
    "auto" => Some(ModuleDetectionKind::Auto),
    "force" => Some(ModuleDetectionKind::Force),
    "legacy" => Some(ModuleDetectionKind::Legacy),
    _ => None,
  }
}

fn canonicalize_module_resolution_value(raw: &str) -> Option<String> {
  match raw.trim() {
    "" => None,
    raw => match raw.to_ascii_lowercase().as_str() {
      "nodejs" | "node10" => Some("node".to_string()),
      other => Some(other.to_string()),
    },
  }
}

fn canonicalize_lib_name(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }

  // TypeScript treats lib names case-insensitively.
  let mut normalized = trimmed.to_ascii_lowercase();

  // Permit passing file names/paths (`lib.es2020.d.ts` or `.../lib.es2020.d.ts`).
  if let Some((_, tail)) = normalized.rsplit_once(['/', '\\']) {
    normalized = tail.to_string();
  }

  normalized = normalized.trim_start_matches("lib.").to_string();
  normalized = normalized
    .trim_end_matches(".d.ts")
    .trim_end_matches(".ts")
    .to_string();

  // `--lib es6` is an alias for `es2015`.
  if normalized == "es6" {
    normalized = "es2015".to_string();
  }

  if normalized.is_empty() {
    return None;
  }

  // TypeScript lib names are dot-separated ASCII identifiers.
  if !normalized
    .chars()
    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.')
  {
    return None;
  }

  Some(normalized)
}

fn normalize_lib_option_value(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }

  // TypeScript treats lib names case-insensitively.
  let mut normalized = trimmed.to_ascii_lowercase();

  // Permit passing file names/paths (`lib.es2020.d.ts` or `.../lib.es2020.d.ts`).
  if let Some((_, tail)) = normalized.rsplit_once(['/', '\\']) {
    normalized = tail.to_string();
  }

  let mut candidate = normalized.trim_start_matches("lib.").to_string();
  candidate = candidate
    .trim_end_matches(".d.ts")
    .trim_end_matches(".ts")
    .to_string();

  // `--lib es6` is an alias for `es2015`.
  if candidate == "es6" {
    candidate = "es2015".to_string();
  }

  if !candidate.is_empty() {
    return Some(candidate);
  }

  // If stripping `lib.` / suffixes produced an empty string, preserve the
  // original value so downstream validation can produce a deterministic TS6046
  // diagnostic.
  Some(normalized)
}

/// Ordered set of libs to load.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LibSet {
  libs: Vec<LibName>,
}

impl LibSet {
  pub fn empty() -> Self {
    LibSet { libs: Vec::new() }
  }

  /// Compute the default lib set for a given compiler configuration.
  pub fn for_options(options: &CompilerOptions) -> Self {
    // TypeScript's `compilerOptions.lib` replaces the default library set
    // entirely (including the baseline `es5`/`es2015` lib implied by `target`).
    //
    // This means specifying `lib` without a foundational ES lib produces
    // `TS2318` diagnostics ("Cannot find global type ...") because core global
    // types like `Array`/`String` are missing.
    if !options.libs.is_empty() {
      return LibSet::from(options.libs.clone());
    }

    if options.no_default_lib {
      return LibSet::empty();
    }

    LibSet::from(default_libs_for_target(options.target))
  }

  pub fn libs(&self) -> &[LibName] {
    &self.libs
  }
}

impl fmt::Display for LibSet {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let names: Vec<_> = self.libs.iter().map(|l| l.as_str()).collect();
    write!(f, "{}", names.join(", "))
  }
}

impl From<Vec<LibName>> for LibSet {
  fn from(libs: Vec<LibName>) -> Self {
    let mut libs = libs;
    libs.sort();
    libs.dedup();
    LibSet { libs }
  }
}

#[cfg(feature = "bundled-libs")]
fn default_libs_for_target(target: ScriptTarget) -> Vec<LibName> {
  let entry_text = match target {
    ScriptTarget::Es3 | ScriptTarget::Es5 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.d.ts"
    )),
    ScriptTarget::Es2015 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es6.d.ts"
    )),
    ScriptTarget::Es2016 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2016.full.d.ts"
    )),
    ScriptTarget::Es2017 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2017.full.d.ts"
    )),
    ScriptTarget::Es2018 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2018.full.d.ts"
    )),
    ScriptTarget::Es2019 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2019.full.d.ts"
    )),
    ScriptTarget::Es2020 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2020.full.d.ts"
    )),
    ScriptTarget::Es2021 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2021.full.d.ts"
    )),
    ScriptTarget::Es2022 => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.es2022.full.d.ts"
    )),
    ScriptTarget::EsNext => include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/fixtures/typescript-libs/5.9.3/lib.esnext.full.d.ts"
    )),
  };

  referenced_lib_option_names(entry_text)
}

#[cfg(not(feature = "bundled-libs"))]
fn default_libs_for_target(target: ScriptTarget) -> Vec<LibName> {
  let mut libs = vec![
    match target {
      ScriptTarget::Es3 | ScriptTarget::Es5 => lib_name("es5"),
      ScriptTarget::Es2015 => lib_name("es2015"),
      ScriptTarget::Es2016 => lib_name("es2016"),
      ScriptTarget::Es2017 => lib_name("es2017"),
      ScriptTarget::Es2018 => lib_name("es2018"),
      ScriptTarget::Es2019 => lib_name("es2019"),
      ScriptTarget::Es2020 => lib_name("es2020"),
      ScriptTarget::Es2021 => lib_name("es2021"),
      ScriptTarget::Es2022 => lib_name("es2022"),
      ScriptTarget::EsNext => lib_name("esnext"),
    },
    lib_name("dom"),
    lib_name("webworker.importscripts"),
    lib_name("scripthost"),
  ];

  if matches!(
    target,
    ScriptTarget::Es2015
      | ScriptTarget::Es2016
      | ScriptTarget::Es2017
      | ScriptTarget::Es2018
      | ScriptTarget::Es2019
      | ScriptTarget::Es2020
      | ScriptTarget::Es2021
      | ScriptTarget::Es2022
      | ScriptTarget::EsNext
  ) {
    libs.push(lib_name("dom.iterable"));
  }

  if matches!(
    target,
    ScriptTarget::Es2018
      | ScriptTarget::Es2019
      | ScriptTarget::Es2020
      | ScriptTarget::Es2021
      | ScriptTarget::Es2022
      | ScriptTarget::EsNext
  ) {
    libs.push(lib_name("dom.asynciterable"));
  }

  libs
}

#[cfg(feature = "bundled-libs")]
fn referenced_lib_option_names(text: &str) -> Vec<LibName> {
  fn attr_value<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let mut offset = 0;
    while let Some(found) = line[offset..].find(needle) {
      let start = offset + found;
      if start == 0 || line.as_bytes()[start - 1].is_ascii_whitespace() {
        let value_start = start + needle.len();
        let rest = &line[value_start..];
        let end = rest.find('"')?;
        return Some(&rest[..end]);
      }
      offset = start + needle.len();
    }
    None
  }

  let mut out = Vec::new();
  let mut in_directives = false;
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() {
      continue;
    }
    if !line.starts_with("///") {
      if in_directives {
        break;
      }
      continue;
    }
    in_directives = true;

    if let Some(lib_name) = attr_value(line, "lib=\"") {
      let parsed = LibName::from_option_name(lib_name)
        .unwrap_or_else(|| panic!("invalid lib reference: {lib_name}"));
      out.push(parsed);
    }
  }

  out
}

#[cfg(any(test, not(feature = "bundled-libs")))]
fn lib_name(name: &'static str) -> LibName {
  LibName(Arc::from(name))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn canonicalizes_lib_names() {
    let cases = [
      ("es2020", "es2020"),
      ("ES2020", "es2020"),
      ("lib.es2020.d.ts", "es2020"),
      ("lib.es2020.ts", "es2020"),
      ("dom.iterable", "dom.iterable"),
      ("LIB.DOM.ITERABLE.D.TS", "dom.iterable"),
      ("webworker.importscripts", "webworker.importscripts"),
      ("scripthost", "scripthost"),
      ("esnext.disposable", "esnext.disposable"),
      ("lib.esnext.disposable.d.ts", "esnext.disposable"),
      ("es6", "es2015"),
      ("lib.es6.d.ts", "es2015"),
      ("path/to/lib.es2020.d.ts", "es2020"),
      ("path\\to\\lib.es2020.d.ts", "es2020"),
    ];

    for (raw, expected) in cases {
      let parsed = LibName::parse(raw).unwrap_or_else(|| panic!("expected parse for {raw}"));
      assert_eq!(parsed.as_str(), expected);
    }

    assert!(LibName::parse("").is_none());
    assert!(LibName::parse("lib.").is_none());
    assert!(LibName::parse("es2020+dom").is_none());
  }

  #[test]
  fn computes_default_libs_from_target() {
    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    let libs = LibSet::for_options(&options);
    assert_eq!(
      libs.libs(),
      &[
        lib_name("dom"),
        lib_name("es5"),
        lib_name("scripthost"),
        lib_name("webworker.importscripts")
      ],
      "es5 defaults should include the host/environment libs"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    let libs = LibSet::for_options(&options);
    assert_eq!(
      libs.libs(),
      &[
        lib_name("dom"),
        lib_name("dom.iterable"),
        lib_name("es2015"),
        lib_name("scripthost"),
        lib_name("webworker.importscripts")
      ],
      "es2015 defaults should include dom.iterable"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2018;
    let libs = LibSet::for_options(&options);
    assert_eq!(
      libs.libs(),
      &[
        lib_name("dom"),
        lib_name("dom.asynciterable"),
        lib_name("dom.iterable"),
        lib_name("es2018"),
        lib_name("scripthost"),
        lib_name("webworker.importscripts")
      ],
      "es2018+ defaults should include dom.asynciterable"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    options.libs = vec![
      LibName::parse("dom.iterable").unwrap(),
      LibName::parse("es2015.promise").unwrap(),
    ];
    let libs = LibSet::for_options(&options);
    assert_eq!(
      libs.libs(),
      &[
        LibName::parse("dom.iterable").unwrap(),
        LibName::parse("es2015.promise").unwrap()
      ],
      "explicit libs should override defaults (no implicit ES lib)"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::EsNext;
    let libs = LibSet::for_options(&options);
    assert_eq!(
      libs.libs(),
      &[
        lib_name("dom"),
        lib_name("dom.asynciterable"),
        lib_name("dom.iterable"),
        lib_name("esnext"),
        lib_name("scripthost"),
        lib_name("webworker.importscripts")
      ],
      "esnext defaults should include env libs but not esnext.disposable explicitly"
    );

    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    let libs = LibSet::for_options(&options);
    assert!(libs.libs().is_empty());

    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.libs = vec![LibName::parse("es2015.promise").unwrap()];
    let libs = LibSet::for_options(&options);
    assert_eq!(libs.libs(), &[LibName::parse("es2015.promise").unwrap()]);
  }

  #[test]
  fn compiler_options_normalization_is_idempotent() {
    let mut options = CompilerOptions::default();
    options.module_resolution = Some("  Node16 ".to_string());
    options.module_detection = Some("  Force ".to_string());
    options.jsx_import_source = Some("  react ".to_string());
    // `strict_native` is a legacy alias for `native_strict`; normalization should
    // make them consistent.
    options.native_strict = true;
    options.strict_native = false;
    options.types = vec![
      " react ".to_string(),
      "".to_string(),
      "react".to_string(),
      "jest".to_string(),
    ];
    options.libs = vec![
      LibName::from_compiler_option_value("ES2020").unwrap(),
      LibName::from_compiler_option_value("dom").unwrap(),
      LibName::from_compiler_option_value("es2020").unwrap(),
    ];

    let once = options.clone().normalize();
    let twice = once.clone().normalize();
    assert_eq!(once, twice);
    assert_eq!(once.module_resolution.as_deref(), Some("node16"));
    assert_eq!(once.module_detection.as_deref(), Some("force"));
    assert_eq!(once.jsx_import_source.as_deref(), Some("react"));
    assert!(once.native_strict);
    assert!(once.strict_native);
    assert_eq!(once.types, vec!["jest".to_string(), "react".to_string()]);
    assert_eq!(
      once.libs,
      vec![
        LibName::from_compiler_option_value("dom").unwrap(),
        LibName::from_compiler_option_value("es2020").unwrap(),
      ]
    );
  }

  #[test]
  fn compiler_options_normalization_is_order_insensitive() {
    let mut a = CompilerOptions::default();
    a.module_resolution = Some("NODE".to_string());
    a.module_detection = Some(" AUTO ".to_string());
    a.jsx_import_source = Some(" react ".to_string());
    a.types = vec!["b".to_string(), "a".to_string()];
    a.libs = vec![
      LibName::from_compiler_option_value("es2020").unwrap(),
      LibName::from_compiler_option_value("dom").unwrap(),
    ];

    let mut b = CompilerOptions::default();
    b.module_resolution = Some(" node ".to_string());
    b.module_detection = Some("auto".to_string());
    b.jsx_import_source = Some("react".to_string());
    b.types = vec!["a".to_string(), "b".to_string(), "b".to_string()];
    b.libs = vec![
      LibName::from_compiler_option_value("DOM").unwrap(),
      LibName::from_compiler_option_value("ES2020").unwrap(),
      LibName::from_compiler_option_value("es2020").unwrap(),
    ];

    assert_eq!(a.normalize(), b.normalize());
  }

  #[test]
  fn compiler_options_validation_is_deterministic_and_applies_fixups() {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.libs = vec![
      LibName::from_compiler_option_value("dom").unwrap(),
      LibName::from_compiler_option_value("es2020").unwrap(),
    ];

    let (normalized, diagnostics) = options.normalize_and_validate();
    assert!(normalized.libs.is_empty(), "libs should be ignored under noLib");
    assert_eq!(
      diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect::<Vec<_>>(),
      vec!["TS5053"]
    );

    // Re-validating should not emit duplicate diagnostics.
    let (normalized2, diagnostics2) = normalized.clone().normalize_and_validate();
    assert_eq!(normalized, normalized2);
    assert!(diagnostics2.is_empty());
  }

  #[test]
  fn effective_module_kind_defaults_from_target() {
    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    options.module = None;
    assert_eq!(options.effective_module_kind(), ModuleKind::CommonJs);

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    options.module = None;
    assert_eq!(options.effective_module_kind(), ModuleKind::Es2015);

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::EsNext;
    options.module = None;
    assert_eq!(options.effective_module_kind(), ModuleKind::Es2015);

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    options.module = Some(ModuleKind::EsNext);
    assert_eq!(options.effective_module_kind(), ModuleKind::EsNext);
  }

  #[test]
  fn effective_module_resolution_defaults_from_module_kind() {
    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::None);
    options.module_resolution = None;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Classic
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::Umd);
    options.module_resolution = None;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Classic
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::Node16);
    options.module_resolution = None;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Node16
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::NodeNext);
    options.module_resolution = None;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::NodeNext
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::EsNext);
    options.module_resolution = None;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Bundler
    );
  }

  #[test]
  fn invalid_module_resolution_emits_ts6046_and_is_fixed_up() {
    let mut options = CompilerOptions::default();
    options.module_resolution = Some("definitely-not-a-mode".to_string());

    let (normalized, diagnostics) = options.normalize_and_validate();
    assert_eq!(
      diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect::<Vec<_>>(),
      vec!["TS6046"]
    );
    assert!(normalized.module_resolution.is_none());

    let (normalized2, diagnostics2) = normalized.clone().normalize_and_validate();
    assert_eq!(normalized, normalized2);
    assert!(diagnostics2.is_empty());
  }

  #[test]
  fn incompatible_module_and_module_resolution_emits_ts5110() {
    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::EsNext);
    options.module_resolution = Some("node16".to_string());

    let (normalized, diagnostics) = options.normalize_and_validate();
    assert!(
      diagnostics.iter().any(|d| d.code.as_str() == "TS5110"),
      "expected TS5110 diagnostic, got: {diagnostics:?}"
    );
    assert_eq!(normalized.module, Some(ModuleKind::EsNext));
    assert_eq!(normalized.module_resolution.as_deref(), Some("node16"));
  }

  #[test]
  fn invalid_libs_produce_deduped_ts6046_diagnostics() {
    let mut options = CompilerOptions::default();
    options.libs = vec![
      LibName::from_compiler_option_value("definitely-not-a-lib").unwrap(),
      LibName::from_compiler_option_value("DEFINITELY-NOT-A-LIB").unwrap(),
      LibName::from_compiler_option_value("es2020").unwrap(),
    ];

    let options = options.normalize();
    let manager = crate::lib_support::lib_env::LibManager::new();
    let loaded = manager.bundled_libs(&options);
    let invalid: Vec<_> = loaded
      .diagnostics
      .iter()
      .filter(|d| d.code.as_str() == crate::codes::INVALID_LIB_OPTION.as_str())
      .collect();
    assert_eq!(invalid.len(), 1, "expected TS6046 to be deduped");
  }

  #[test]
  fn effective_module_kind_defaults_match_tsc() {
    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    assert_eq!(
      effective_module_kind(&options),
      ModuleKind::CommonJs,
      "tsc defaults `module` to CommonJS for ES5 targets"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    assert_eq!(
      effective_module_kind(&options),
      ModuleKind::Es2015,
      "tsc defaults `module` to ES2015 for ES2015+ targets"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2022;
    assert_eq!(
      effective_module_kind(&options),
      ModuleKind::Es2015,
      "tsc keeps the default module kind pinned to ES2015 even for newer targets"
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    options.module = Some(ModuleKind::Amd);
    assert_eq!(
      effective_module_kind(&options),
      ModuleKind::Amd,
      "explicit module kind should override target-derived defaults"
    );
  }

  #[test]
  fn effective_module_resolution_mode_defaults_match_tsc() {
    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es5;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Bundler
    );

    let mut options = CompilerOptions::default();
    options.target = ScriptTarget::Es2015;
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Bundler
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::None);
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Classic
    );

    for module in [ModuleKind::Amd, ModuleKind::Umd, ModuleKind::System] {
      let mut options = CompilerOptions::default();
      options.module = Some(module);
      assert_eq!(
        options.effective_module_resolution(),
        ModuleResolutionKind::Classic
      );
    }

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::Node16);
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Node16
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::NodeNext);
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::NodeNext
    );
  }

  #[test]
  fn effective_module_resolution_mode_parses_strings_and_aliases() {
    let mut options = CompilerOptions::default();
    options.module_resolution = Some(" classic ".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Classic
    );

    for raw in ["node", "Node", "nodejs", "NODEJS", "node10", " NODE10 "] {
      let mut options = CompilerOptions::default();
      options.module_resolution = Some(raw.to_string());
      assert_eq!(
        options.effective_module_resolution(),
        ModuleResolutionKind::Node10,
        "expected {raw:?} to map to Node10"
      );
    }

    let mut options = CompilerOptions::default();
    options.module_resolution = Some("Node16".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Node16
    );

    let mut options = CompilerOptions::default();
    options.module_resolution = Some("NodeNext".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::NodeNext
    );

    let mut options = CompilerOptions::default();
    options.module_resolution = Some("bundler".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Bundler
    );
  }

  #[test]
  fn effective_module_resolution_mode_treats_unknown_values_as_unset() {
    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::NodeNext);
    options.module_resolution = Some("not-a-real-mode".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::NodeNext,
      "unknown values should fall back to computed defaults derived from module kind"
    );

    let mut options = CompilerOptions::default();
    options.module = Some(ModuleKind::None);
    options.module_resolution = Some("not-a-real-mode".to_string());
    assert_eq!(
      options.effective_module_resolution(),
      ModuleResolutionKind::Classic,
      "unknown values should fall back to computed defaults derived from module kind"
    );
  }
}

impl From<&CompilerOptions> for types_ts_interned::TypeOptions {
  fn from(options: &CompilerOptions) -> Self {
    types_ts_interned::TypeOptions {
      strict_null_checks: options.strict_null_checks,
      strict_function_types: options.strict_function_types,
      exact_optional_property_types: options.exact_optional_property_types,
      no_unchecked_indexed_access: options.no_unchecked_indexed_access,
    }
  }
}
