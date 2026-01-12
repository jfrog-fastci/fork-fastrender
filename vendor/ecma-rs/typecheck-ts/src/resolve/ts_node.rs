//! Deterministic Node/TypeScript-style module resolution.
//!
//! This is a mostly direct port of the resolver used by `typecheck-ts-harness`,
//! adapted to operate over a pluggable [`ResolveFs`] implementation so it can be
//! reused by real hosts (CLI/disk) and in-memory tests.

use diagnostics::paths::normalize_ts_path_into;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::path::canonicalize_path;
use crate::lib_support::ModuleKind;
use crate::resolve::path::normalize_path;

const CONDITIONS_NODE10_IMPORT: [&str; 4] = ["types", "import", "require", "default"];
const CONDITIONS_NODE10_REQUIRE: [&str; 4] = ["types", "require", "import", "default"];
const CONDITIONS_NODE16_IMPORT: [&str; 3] = ["types", "import", "default"];
const CONDITIONS_NODE16_REQUIRE: [&str; 3] = ["types", "require", "default"];
const CONDITIONS_BUNDLER: [&str; 3] = ["types", "import", "default"];

/// TypeScript-aware extension search order for module resolution.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
  "ts", "tsx", "d.ts", "mts", "d.mts", "cts", "d.cts", "js", "jsx", "mjs", "cjs",
];

const INDEX_FILES: [&str; 11] = [
  "index.ts",
  "index.tsx",
  "index.d.ts",
  "index.mts",
  "index.d.mts",
  "index.cts",
  "index.d.cts",
  "index.js",
  "index.jsx",
  "index.mjs",
  "index.cjs",
];

/// TypeScript's `moduleResolution` modes that affect package.json resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModuleResolutionMode {
  /// TypeScript's legacy "classic" resolution algorithm.
  ///
  /// This mode does **not** search `node_modules/` or consult package.json
  /// `exports`/`imports` maps for bare specifiers. See `resolve_non_relative`
  /// for the classic algorithm implementation.
  Classic,
  #[default]
  Node10,
  Node16,
  NodeNext,
  Bundler,
}

/// Whether a module specifier is being resolved from an `import`-like or
/// `require`-like context (affects conditional exports selection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResolutionKind {
  #[default]
  Import,
  Require,
}

/// Minimal semver version used for `typesVersions` range selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypeScriptVersion {
  pub major: u32,
  pub minor: u32,
  pub patch: u32,
}

impl TypeScriptVersion {
  pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
    Self { major, minor, patch }
  }
}

impl Default for TypeScriptVersion {
  fn default() -> Self {
    // Keep in sync with `typecheck-ts/build.rs` which pins the bundled lib
    // TypeScript version.
    TypeScriptVersion::new(5, 9, 3)
  }
}

/// Options controlling module resolution behaviour.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResolveOptions {
  /// Whether to walk `node_modules/` when resolving bare specifiers.
  pub node_modules: bool,
  /// Whether to resolve `#imports` specifiers using the nearest package.json.
  pub package_imports: bool,
  /// TypeScript `moduleResolution` mode (affects exports conditions and `typesVersions`).
  pub module_resolution: ModuleResolutionMode,
  /// TypeScript `module` kind for inferring import vs require context.
  pub module_kind: Option<ModuleKind>,
  /// TypeScript compiler version used for `typesVersions` range selection.
  ///
  /// Note: defaults to the workspace-pinned TypeScript version.
  pub typescript_version: TypeScriptVersion,
}

/// Filesystem abstraction for resolution to allow testing and non-disk hosts.
pub trait ResolveFs: Clone {
  /// Return true if the path points to a file.
  fn is_file(&self, path: &Path) -> bool;
  /// Return true if the path points to a directory.
  fn is_dir(&self, path: &Path) -> bool;
  /// Read a UTF-8 file from disk.
  fn read_to_string(&self, _path: &Path) -> Option<String> {
    None
  }
  /// Canonicalise a path into an absolute, platform path.
  fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
}

/// Real filesystem adapter used by the CLI.
#[derive(Clone, Debug, Default)]
pub struct RealFs;

impl ResolveFs for RealFs {
  fn is_file(&self, path: &Path) -> bool {
    std::fs::metadata(path)
      .map(|m| m.is_file())
      .unwrap_or(false)
  }

  fn is_dir(&self, path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
  }

  fn read_to_string(&self, path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
  }

  fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
    canonicalize_path(path).ok()
  }
}

#[derive(Clone, Debug)]
struct PackageJson {
  value: Value,
  types_versions: Option<Vec<(String, Value)>>,
}

impl Deref for PackageJson {
  type Target = Value;

  fn deref(&self) -> &Self::Target {
    &self.value
  }
}

#[derive(Debug, Deserialize)]
struct PackageJsonTypesVersions {
  #[serde(
    rename = "typesVersions",
    default,
    deserialize_with = "deserialize_optional_ordered_object",
  )]
  types_versions: Option<Vec<(String, Value)>>,
}

fn deserialize_optional_ordered_object<'de, D>(
  deserializer: D,
) -> Result<Option<Vec<(String, Value)>>, D::Error>
where
  D: serde::Deserializer<'de>,
{
  struct OrderedObjectVisitor;

  impl<'de> Visitor<'de> for OrderedObjectVisitor {
    type Value = Option<Vec<(String, Value)>>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
      formatter.write_str("a JSON object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
      M: MapAccess<'de>,
    {
      let mut entries = Vec::new();
      while let Some((key, value)) = map.next_entry::<String, Value>()? {
        entries.push((key, value));
      }
      Ok(Some(entries))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_string<E>(self, _v: String) -> Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(None)
    }

    fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
    where
      A: SeqAccess<'de>,
    {
      Ok(None)
    }
  }

  deserializer.deserialize_any(OrderedObjectVisitor)
}

/// Deterministic resolver implementing TypeScript's Node-style module resolution
/// (package.json `exports` / `types` / `imports`, extension probing, node_modules).
#[derive(Clone, Debug)]
pub struct Resolver<F = RealFs> {
  fs: F,
  options: ResolveOptions,
  package_json_cache: Arc<Mutex<HashMap<PathBuf, Option<Arc<PackageJson>>>>>,
}

impl Resolver<RealFs> {
  /// Construct a resolver that reads from disk.
  pub fn new(options: ResolveOptions) -> Self {
    Self {
      fs: RealFs,
      options,
      package_json_cache: Arc::new(Mutex::new(HashMap::new())),
    }
  }
}

impl<F: ResolveFs> Resolver<F> {
  /// Construct a resolver with a custom filesystem implementation.
  pub fn with_fs(fs: F, options: ResolveOptions) -> Self {
    Self {
      fs,
      options,
      package_json_cache: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Resolve a module specifier relative to `from`.
  pub fn resolve(&self, from: &Path, specifier: &str) -> Option<PathBuf> {
    let from_name = normalize_path(from);
    let kind = self.infer_resolution_kind(&from_name);
    self.resolve_inner(&from_name, specifier, kind)
  }

  /// Resolve a module specifier relative to `from`, using an explicit `import`/`require` context.
  pub fn resolve_with_kind(
    &self,
    from: &Path,
    specifier: &str,
    kind: ResolutionKind,
  ) -> Option<PathBuf> {
    let from_name = normalize_path(from);
    self.resolve_inner(&from_name, specifier, kind)
  }

  fn resolve_inner(&self, from: &str, specifier: &str, kind: ResolutionKind) -> Option<PathBuf> {
    let conditions = self.export_conditions(kind);
    if is_relative_specifier(specifier) {
      return self.resolve_relative(from, specifier, conditions);
    }
    self.resolve_non_relative(from, specifier, conditions)
  }

  fn export_conditions(&self, kind: ResolutionKind) -> &'static [&'static str] {
    match self.options.module_resolution {
      ModuleResolutionMode::Node16 | ModuleResolutionMode::NodeNext => match kind {
        ResolutionKind::Import => &CONDITIONS_NODE16_IMPORT,
        ResolutionKind::Require => &CONDITIONS_NODE16_REQUIRE,
      },
      ModuleResolutionMode::Bundler => &CONDITIONS_BUNDLER,
      ModuleResolutionMode::Classic | ModuleResolutionMode::Node10 => match kind {
        ResolutionKind::Import => &CONDITIONS_NODE10_IMPORT,
        ResolutionKind::Require => &CONDITIONS_NODE10_REQUIRE,
      },
    }
  }

  fn infer_resolution_kind(&self, from: &str) -> ResolutionKind {
    // File extensions that force a module format.
    if from.ends_with(".cts") || from.ends_with(".cjs") || from.ends_with(".d.cts") {
      return ResolutionKind::Require;
    }
    if from.ends_with(".mts") || from.ends_with(".mjs") || from.ends_with(".d.mts") {
      return ResolutionKind::Import;
    }

    // The compiler `module` option reflects the source module system more directly than
    // `moduleResolution` and helps disambiguate `.ts` files.
    match self.options.module_kind {
      Some(ModuleKind::CommonJs) => return ResolutionKind::Require,
      Some(ModuleKind::Node16) => return ResolutionKind::Require,
      Some(ModuleKind::NodeNext) => return ResolutionKind::Import,
      _ => {}
    }

    // Fall back to `moduleResolution` defaults when no stronger signal is available.
    match self.options.module_resolution {
      ModuleResolutionMode::Node16 => ResolutionKind::Require,
      ModuleResolutionMode::NodeNext | ModuleResolutionMode::Bundler => ResolutionKind::Import,
      ModuleResolutionMode::Classic | ModuleResolutionMode::Node10 => ResolutionKind::Import,
    }
  }

  fn resolve_relative(
    &self,
    from: &str,
    specifier: &str,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    let parent = virtual_parent_dir_str(from);
    let mut resolve_scratch = String::new();

    if let Some(entry) = specifier.strip_prefix("./") {
      if entry.is_empty() {
        return self.resolve_as_file_or_directory_normalized_with_scratch(
          parent,
          0,
          &mut resolve_scratch,
          conditions,
        );
      }

      let mut joined = virtual_join(parent, entry);
      return if entry.starts_with('/') || subpath_needs_normalization(entry) {
        normalize_ts_path_into(&joined, &mut resolve_scratch);
        self.resolve_as_file_or_directory_normalized_with_scratch(
          &resolve_scratch,
          0,
          &mut joined,
          conditions,
        )
      } else {
        self.resolve_as_file_or_directory_normalized_with_scratch(
          &joined,
          0,
          &mut resolve_scratch,
          conditions,
        )
      };
    }

    let mut joined = virtual_join(parent, specifier);
    normalize_ts_path_into(&joined, &mut resolve_scratch);
    self.resolve_as_file_or_directory_normalized_with_scratch(&resolve_scratch, 0, &mut joined, conditions)
  }

  fn resolve_non_relative(&self, from: &str, specifier: &str, conditions: &[&str]) -> Option<PathBuf> {
    let mut resolve_scratch = String::new();

    if specifier.starts_with('#') {
      if self.options.module_resolution == ModuleResolutionMode::Classic {
        return None;
      }
      if !self.options.package_imports {
        return None;
      }
      return self.resolve_imports_specifier(from, specifier, conditions);
    }

    if is_source_root(specifier)
      || specifier.starts_with('/')
      || specifier.starts_with('\\')
      || starts_with_drive_letter(specifier)
    {
      let normalized = diagnostics::paths::normalize_ts_path(specifier);
      if let Some(found) = self.resolve_as_file_or_directory_normalized_with_scratch(
        &normalized,
        0,
        &mut resolve_scratch,
        conditions,
      ) {
        return Some(found);
      }
    }

    if self.options.module_resolution == ModuleResolutionMode::Classic {
      return self.resolve_non_relative_classic(from, specifier);
    }

    if !self.options.node_modules {
      return None;
    }

    let (package_name, package_rest) = split_package_name(specifier).unwrap_or((specifier, ""));
    let subpath = package_rest.trim_start_matches('/');
    let exports_subpath = (!subpath.is_empty()).then(|| {
      let mut resolved = String::with_capacity(2 + subpath.len());
      resolved.push('.');
      resolved.push('/');
      resolved.push_str(subpath);
      resolved
    });

    let mut types_specifier: Option<Cow<'_, str>> = None;
    let mut types_specifier_checked = false;

    let mut dir = virtual_parent_dir_str(from);
    let mut package_dir = String::with_capacity(
      dir.len() + 2 + "node_modules".len() + package_name.len() + subpath.len(),
    );
    let mut types_base = String::with_capacity(
      dir.len() + 2 + "node_modules/@types".len() + specifier.len() + subpath.len(),
    );

    loop {
      virtual_join3_into(&mut package_dir, dir, "node_modules", package_name);
      let package_dir_len = package_dir.len();
      if let Some(exports_subpath) = exports_subpath.as_deref() {
        virtual_join_into(&mut types_base, &package_dir, "package.json");
        if self.fs.is_file(Path::new(types_base.as_str())) {
          if let Some(parsed) = self.package_json(&types_base) {
            if let Some(exports) = parsed.get("exports") {
              if let Some((target, star_match)) = select_exports_target(exports, exports_subpath) {
                if let Some(found) = self.resolve_json_target_to_file(
                  &package_dir,
                  target,
                  star_match,
                  0,
                  &mut types_base,
                  &mut resolve_scratch,
                  conditions,
                ) {
                  return Some(found);
                }
              }
            }

            if let Some(found) =
              self.resolve_types_versions(&package_dir, parsed.as_ref(), subpath, 0, &mut types_base, &mut resolve_scratch, conditions)
            {
              return Some(found);
            }
          }
        }
        package_dir.push('/');
        package_dir.push_str(subpath);
        let found = if subpath_needs_normalization(subpath) {
          normalize_ts_path_into(&package_dir, &mut resolve_scratch);
          self.resolve_as_file_or_directory_normalized_with_scratch(
            &resolve_scratch,
            0,
            &mut types_base,
            conditions,
          )
        } else {
          self.resolve_as_file_or_directory_normalized_with_scratch(
            &package_dir,
            0,
            &mut resolve_scratch,
            conditions,
          )
        };
        package_dir.truncate(package_dir_len);
        if let Some(found) = found {
          return Some(found);
        }
      } else if let Some(found) = self.resolve_as_file_or_directory_normalized_with_scratch(
        &package_dir,
        0,
        &mut resolve_scratch,
        conditions,
      ) {
        return Some(found);
      }

      if !types_specifier_checked {
        types_specifier = types_fallback_specifier(specifier);
        types_specifier_checked = true;
      }
      if let Some(types_specifier) = types_specifier.as_deref() {
        virtual_join3_into(&mut types_base, dir, "node_modules/@types", types_specifier);
        if let Some(found) = self.resolve_as_file_or_directory_normalized_with_scratch(
          &types_base,
          0,
          &mut resolve_scratch,
          conditions,
        ) {
          return Some(found);
        }
      }

      let parent = virtual_parent_dir_str(dir);
      if parent == dir {
        break;
      }
      dir = parent;
    }

    None
  }

  fn resolve_non_relative_classic(&self, from: &str, specifier: &str) -> Option<PathBuf> {
    let needs_normalization = subpath_needs_normalization(specifier);
    let mut dir = virtual_parent_dir_str(from);
    let mut candidate = String::new();
    let mut normalized = String::new();
    let mut scratch = String::new();

    loop {
      virtual_join_into(&mut candidate, dir, specifier);
      let found = if needs_normalization {
        normalize_ts_path_into(&candidate, &mut normalized);
        self.resolve_as_file_or_directory_no_package_json_normalized_with_scratch(&normalized, &mut scratch)
      } else {
        self.resolve_as_file_or_directory_no_package_json_normalized_with_scratch(&candidate, &mut scratch)
      };
      if found.is_some() {
        return found;
      }

      let parent = virtual_parent_dir_str(dir);
      if parent == dir {
        break;
      }
      dir = parent;
    }

    None
  }

  fn resolve_imports_specifier(
    &self,
    from: &str,
    specifier: &str,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    let mut dir = virtual_parent_dir_str(from);
    let mut package_json_path = String::with_capacity(dir.len() + 1 + "package.json".len());
    let mut resolve_scratch = String::new();
    loop {
      virtual_join_into(&mut package_json_path, dir, "package.json");
      if let Some(found) =
        self.resolve_imports_in_dir(dir, &mut package_json_path, &mut resolve_scratch, specifier, conditions)
      {
        return Some(found);
      }

      let parent = virtual_parent_dir_str(dir);
      if parent == dir {
        break;
      }
      dir = parent;
    }

    None
  }

  fn resolve_imports_in_dir(
    &self,
    dir: &str,
    package_json_path: &mut String,
    resolve_scratch: &mut String,
    specifier: &str,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    let parsed = self.package_json(package_json_path.as_str())?;
    let imports = parsed.get("imports")?.as_object()?;

    let (target, star_match) = if let Some(target) = imports.get(specifier) {
      (target, None)
    } else {
      let (pattern_key, star_match) = best_exports_subpath_pattern(imports, specifier)?;
      (imports.get(pattern_key)?, Some(star_match))
    };

    self.resolve_json_target_to_file(
      dir,
      target,
      star_match,
      0,
      package_json_path,
      resolve_scratch,
      conditions,
    )
  }

  fn resolve_as_file_or_directory_no_package_json_normalized_with_scratch(
    &self,
    base_candidate: &str,
    scratch: &mut String,
  ) -> Option<PathBuf> {
    self.resolve_as_file_or_directory_impl(base_candidate, 0, scratch, &[], false)
  }

  fn resolve_as_file_or_directory_normalized_with_scratch(
    &self,
    base_candidate: &str,
    depth: usize,
    scratch: &mut String,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    self.resolve_as_file_or_directory_impl(base_candidate, depth, scratch, conditions, true)
  }

  fn resolve_as_file_or_directory_impl(
    &self,
    base_candidate: &str,
    depth: usize,
    scratch: &mut String,
    conditions: &[&str],
    use_package_json: bool,
  ) -> Option<PathBuf> {
    if depth > 16 {
      return None;
    }

    if let Some(found) = self.try_file(base_candidate) {
      return Some(found);
    }

    let base_is_source_root = is_source_root(base_candidate);

    if base_candidate.ends_with(".js") {
      let trimmed = base_candidate.trim_end_matches(".js");
      scratch.clear();
      scratch.push_str(trimmed);
      scratch.push('.');
      let prefix_len = scratch.len();
      for ext in ["ts", "tsx", "d.ts"] {
        scratch.truncate(prefix_len);
        scratch.push_str(ext);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    } else if base_candidate.ends_with(".jsx") {
      let trimmed = base_candidate.trim_end_matches(".jsx");
      scratch.clear();
      scratch.push_str(trimmed);
      scratch.push('.');
      let prefix_len = scratch.len();
      for ext in ["tsx", "d.ts"] {
        scratch.truncate(prefix_len);
        scratch.push_str(ext);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    } else if base_candidate.ends_with(".mjs") {
      let trimmed = base_candidate.trim_end_matches(".mjs");
      scratch.clear();
      scratch.push_str(trimmed);
      scratch.push('.');
      let prefix_len = scratch.len();
      for ext in ["mts", "d.mts"] {
        scratch.truncate(prefix_len);
        scratch.push_str(ext);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    } else if base_candidate.ends_with(".cjs") {
      let trimmed = base_candidate.trim_end_matches(".cjs");
      scratch.clear();
      scratch.push_str(trimmed);
      scratch.push('.');
      let prefix_len = scratch.len();
      for ext in ["cts", "d.cts"] {
        scratch.truncate(prefix_len);
        scratch.push_str(ext);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    } else if !base_is_source_root {
      scratch.clear();
      scratch.push_str(base_candidate);
      scratch.push('.');
      let prefix_len = scratch.len();
      for ext in DEFAULT_EXTENSIONS {
        scratch.truncate(prefix_len);
        scratch.push_str(ext);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    }

    if use_package_json && !base_is_source_root && self.options.node_modules {
      virtual_join_into(scratch, base_candidate, "package.json");
      if self.fs.is_file(Path::new(scratch.as_str())) {
        if let Some(parsed) = self.package_json(scratch.as_str()) {
          let mut resolve_scratch = String::new();
          let resolve_target = |entry: &str,
                                scratch: &mut String,
                                resolve_scratch: &mut String|
           -> Option<PathBuf> {
            if entry.is_empty() {
              return None;
            }

            if entry.starts_with('/') || entry.starts_with('\\') || starts_with_drive_letter(entry)
            {
              normalize_ts_path_into(entry, scratch);
              return self.resolve_as_file_or_directory_normalized_with_scratch(
                scratch,
                depth + 1,
                resolve_scratch,
                conditions,
              );
            }

            let entry = entry.strip_prefix("./").unwrap_or(entry);
            if entry.is_empty() {
              return self.resolve_as_file_or_directory_normalized_with_scratch(
                base_candidate,
                depth + 1,
                resolve_scratch,
                conditions,
              );
            }

            virtual_join_into(scratch, base_candidate, entry);
            if entry.starts_with('/')
              || entry.starts_with('\\')
              || subpath_needs_normalization(entry)
            {
              normalize_ts_path_into(scratch.as_str(), resolve_scratch);
              self.resolve_as_file_or_directory_normalized_with_scratch(
                resolve_scratch,
                depth + 1,
                scratch,
                conditions,
              )
            } else {
              self.resolve_as_file_or_directory_normalized_with_scratch(
                scratch,
                depth + 1,
                resolve_scratch,
                conditions,
              )
            }
          };

          if let Some(found) = self.resolve_types_versions(
            base_candidate,
            parsed.as_ref(),
            "",
            depth,
            scratch,
            &mut resolve_scratch,
            conditions,
          ) {
            return Some(found);
          }

          if let Some(entry) = parsed.get("types").and_then(|v| v.as_str()) {
            if let Some(found) = resolve_target(entry, scratch, &mut resolve_scratch) {
              return Some(found);
            }
          }

          if let Some(entry) = parsed.get("typings").and_then(|v| v.as_str()) {
            if let Some(found) = resolve_target(entry, scratch, &mut resolve_scratch) {
              return Some(found);
            }
          }

          if let Some(exports) = parsed.get("exports") {
            if let Some((target, star_match)) = select_exports_target(exports, ".") {
              if let Some(found) = self.resolve_json_target_to_file(
                base_candidate,
                target,
                star_match,
                depth,
                scratch,
                &mut resolve_scratch,
                conditions,
              ) {
                return Some(found);
              }
            }
          }

          if let Some(entry) = parsed.get("main").and_then(|v| v.as_str()) {
            if let Some(found) = resolve_target(entry, scratch, &mut resolve_scratch) {
              return Some(found);
            }
          }
        }
      }
    
      scratch.clear();
      scratch.push_str(base_candidate);
      if !base_candidate.ends_with('/') {
        scratch.push('/');
      }
      let prefix_len = scratch.len();
      for index in INDEX_FILES {
        scratch.truncate(prefix_len);
        scratch.push_str(index);
        if let Some(found) = self.try_file(scratch) {
          return Some(found);
        }
      }
    }

    None
  }

  fn resolve_types_versions(
    &self,
    package_dir: &str,
    package_json: &PackageJson,
    subpath: &str,
    depth: usize,
    scratch: &mut String,
    resolve_scratch: &mut String,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    let types_versions = package_json.types_versions.as_deref()?;
    if types_versions.is_empty() {
      return None;
    }

    let ts_version = self.options.typescript_version;
    let mut selected: Option<&Value> = None;
    for (range, paths) in types_versions {
      if !types_versions_range_matches(range, ts_version) {
        continue;
      }
      if paths.as_object().is_none() {
        continue;
      }
      selected = Some(paths);
      break;
    }

    let paths = selected?;
    let paths = paths.as_object()?;
    let (targets, star_match) = if let Some(target) = paths.get(subpath) {
      (target, None)
    } else {
      let (pattern_key, star_match) = best_exports_subpath_pattern(paths, subpath)?;
      (paths.get(pattern_key)?, Some(star_match))
    };

    let targets = targets.as_array()?;
    for target in targets {
      let Some(entry) = target.as_str() else {
        continue;
      };
      let found = match star_match {
        Some(star) if entry.contains('*') => self.resolve_json_string_to_file_with_star(
          package_dir,
          entry,
          star,
          depth + 1,
          scratch,
          resolve_scratch,
          conditions,
        ),
        Some(_) | None => self.resolve_json_string_to_file(
          package_dir,
          entry,
          depth + 1,
          scratch,
          resolve_scratch,
          conditions,
        ),
      };
      if found.is_some() {
        return found;
      }
    }

    None
  }

  fn resolve_json_target_to_file(
    &self,
    base_dir: &str,
    value: &Value,
    star_match: Option<&str>,
    depth: usize,
    scratch: &mut String,
    resolve_scratch: &mut String,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    if depth > 16 {
      return None;
    }

    match value {
      Value::String(s) => match star_match {
        Some(star) if s.contains('*') => self.resolve_json_string_to_file_with_star(
          base_dir,
          s,
          star,
          depth + 1,
          scratch,
          resolve_scratch,
          conditions,
        ),
        Some(_) => {
          self.resolve_json_string_to_file(base_dir, s, depth + 1, scratch, resolve_scratch, conditions)
        }
        None => self.resolve_json_string_to_file(base_dir, s, depth + 1, scratch, resolve_scratch, conditions),
      },
      Value::Array(items) => items.iter().find_map(|item| {
        self.resolve_json_target_to_file(
          base_dir,
          item,
          star_match,
          depth + 1,
          scratch,
          resolve_scratch,
          conditions,
        )
      }),
      Value::Object(map) => conditions.iter().find_map(|cond| {
        map.get(*cond).and_then(|next| {
          self.resolve_json_target_to_file(
            base_dir,
            next,
            star_match,
            depth + 1,
            scratch,
            resolve_scratch,
            conditions,
          )
        })
      }),
      Value::Null => None,
      _ => None,
    }
  }

  fn resolve_json_string_to_file(
    &self,
    base_dir: &str,
    entry: &str,
    depth: usize,
    scratch: &mut String,
    resolve_scratch: &mut String,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    if entry.is_empty() {
      return None;
    }
    if entry.starts_with('/') || entry.starts_with('\\') || starts_with_drive_letter(entry) {
      normalize_ts_path_into(entry, scratch);
      return self.resolve_as_file_or_directory_normalized_with_scratch(
        scratch,
        depth,
        resolve_scratch,
        conditions,
      );
    }

    let entry = entry.strip_prefix("./").unwrap_or(entry);
    if entry.is_empty() {
      return self.resolve_as_file_or_directory_normalized_with_scratch(
        base_dir,
        depth,
        resolve_scratch,
        conditions,
      );
    }

    virtual_join_into(scratch, base_dir, entry);
    if entry.starts_with('/') || subpath_needs_normalization(entry) {
      normalize_ts_path_into(scratch.as_str(), resolve_scratch);
      self.resolve_as_file_or_directory_normalized_with_scratch(resolve_scratch, depth, scratch, conditions)
    } else {
      self.resolve_as_file_or_directory_normalized_with_scratch(scratch, depth, resolve_scratch, conditions)
    }
  }

  fn resolve_json_string_to_file_with_star(
    &self,
    base_dir: &str,
    entry: &str,
    star: &str,
    depth: usize,
    scratch: &mut String,
    resolve_scratch: &mut String,
    conditions: &[&str],
  ) -> Option<PathBuf> {
    if entry.is_empty() {
      return None;
    }

    if entry.starts_with('/') || entry.starts_with('\\') || starts_with_drive_letter(entry) {
      scratch.clear();
      scratch.reserve(entry.len() + star.len());
      push_star_replaced(scratch, entry, star);
      normalize_ts_path_into(scratch.as_str(), resolve_scratch);
      return self.resolve_as_file_or_directory_normalized_with_scratch(
        resolve_scratch,
        depth,
        scratch,
        conditions,
      );
    }

    let stripped = entry.strip_prefix("./").unwrap_or(entry);
    if stripped.is_empty() {
      return self.resolve_as_file_or_directory_normalized_with_scratch(
        base_dir,
        depth,
        resolve_scratch,
        conditions,
      );
    }

    scratch.clear();
    scratch.reserve(base_dir.len() + stripped.len() + star.len() + 1);
    if base_dir == "/" {
      scratch.push('/');
    } else {
      scratch.push_str(base_dir);
      if !base_dir.ends_with('/') {
        scratch.push('/');
      }
    }
    let entry_start = scratch.len();
    push_star_replaced(scratch, stripped, star);
    let replaced_entry = &scratch[entry_start..];

    if replaced_entry.starts_with('/') || subpath_needs_normalization(replaced_entry) {
      normalize_ts_path_into(scratch.as_str(), resolve_scratch);
      self.resolve_as_file_or_directory_normalized_with_scratch(resolve_scratch, depth, scratch, conditions)
    } else {
      self.resolve_as_file_or_directory_normalized_with_scratch(scratch, depth, resolve_scratch, conditions)
    }
  }

  fn try_file(&self, candidate: &str) -> Option<PathBuf> {
    let path = Path::new(candidate);
    if !self.fs.is_file(path) {
      return None;
    }
    let canonical = self.fs.canonicalize(path);
    Some(match canonical {
      Some(path) => PathBuf::from(normalize_path(&path)),
      None => PathBuf::from(candidate),
    })
  }

  fn package_json(&self, path: &str) -> Option<Arc<PackageJson>> {
    let path_buf = PathBuf::from(path);
    {
      let cache = self.package_json_cache.lock().unwrap();
      if let Some(cached) = cache.get(&path_buf) {
        return cached.clone();
      }
    }

    let parsed = if self.fs.is_file(Path::new(path)) {
      let raw = self.fs.read_to_string(Path::new(path))?;
      let value = serde_json::from_str::<Value>(&raw).ok()?;
      let types_versions = serde_json::from_str::<PackageJsonTypesVersions>(&raw)
        .ok()
        .and_then(|parsed| parsed.types_versions);
      Some(Arc::new(PackageJson {
        value,
        types_versions,
      }))
    } else {
      None
    };

    let mut cache = self.package_json_cache.lock().unwrap();
    cache.insert(path_buf, parsed.clone());
    parsed
  }
}

fn push_star_replaced(out: &mut String, template: &str, star: &str) {
  let mut parts = template.split('*');
  if let Some(first) = parts.next() {
    out.push_str(first);
    for part in parts {
      out.push_str(star);
      out.push_str(part);
    }
  }
}

fn types_versions_range_matches(raw: &str, current: TypeScriptVersion) -> bool {
  let raw = raw.trim();
  if raw.is_empty() || raw == "*" {
    return true;
  }
  if raw.contains("||") {
    return false;
  }

  for token in raw.split_whitespace() {
    let (op, version_str) = token
      .strip_prefix(">=")
      .map(|s| (RangeOp::Gte, s))
      .or_else(|| token.strip_prefix('>').map(|s| (RangeOp::Gt, s)))
      .or_else(|| token.strip_prefix("<=").map(|s| (RangeOp::Lte, s)))
      .or_else(|| token.strip_prefix('<').map(|s| (RangeOp::Lt, s)))
      .or_else(|| token.strip_prefix('=').map(|s| (RangeOp::Eq, s)))
      .unwrap_or((RangeOp::Eq, token));
    let Some(version) = parse_typescript_version(version_str) else {
      return false;
    };

    let matches = match op {
      RangeOp::Lt => current < version,
      RangeOp::Lte => current <= version,
      RangeOp::Gt => current > version,
      RangeOp::Gte => current >= version,
      RangeOp::Eq => current == version,
    };
    if !matches {
      return false;
    }
  }

  true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeOp {
  Lt,
  Lte,
  Gt,
  Gte,
  Eq,
}

fn parse_typescript_version(raw: &str) -> Option<TypeScriptVersion> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let mut parts = raw.split('.');
  let major = parts.next()?.parse::<u32>().ok()?;
  let minor = parts.next().unwrap_or("0").parse::<u32>().ok()?;
  let patch_raw = parts.next().unwrap_or("0");
  let end = patch_raw
    .find(|c: char| !c.is_ascii_digit())
    .unwrap_or(patch_raw.len());
  let patch_digits = patch_raw.get(..end).unwrap_or("");
  let patch = if patch_digits.is_empty() {
    0
  } else {
    patch_digits.parse::<u32>().ok()?
  };
  Some(TypeScriptVersion::new(major, minor, patch))
}

fn select_exports_target<'a, 'b>(
  exports: &'a Value,
  subpath: &'b str,
) -> Option<(&'a Value, Option<&'b str>)> {
  match exports {
    Value::Object(map) => {
      let has_subpath_keys = map.keys().next().map_or(false, |k| k.starts_with('.'));
      if has_subpath_keys {
        if let Some(target) = map.get(subpath) {
          return Some((target, None));
        }
        let (pattern_key, star_match) = best_exports_subpath_pattern(map, subpath)?;
        Some((map.get(pattern_key)?, Some(star_match)))
      } else {
        (subpath == ".").then_some((exports, None))
      }
    }
    _ => (subpath == ".").then_some((exports, None)),
  }
}

fn best_exports_subpath_pattern<'a, 'b>(
  map: &'a Map<String, Value>,
  subpath: &'b str,
) -> Option<(&'a str, &'b str)> {
  let mut best_key: Option<&'a str> = None;
  let mut best_star: Option<&'b str> = None;

  for key in map.keys() {
    let Some((prefix, suffix)) = key.split_once('*') else {
      continue;
    };
    if suffix.contains('*') {
      continue;
    }
    if !subpath.starts_with(prefix) || !subpath.ends_with(suffix) {
      continue;
    }
    if subpath.len() < prefix.len() + suffix.len() {
      continue;
    }
    let star = &subpath[prefix.len()..subpath.len() - suffix.len()];

    let replace = match best_key {
      None => true,
      Some(existing) => {
        key.len() > existing.len() || (key.len() == existing.len() && key.as_str() < existing)
      }
    };
    if replace {
      best_key = Some(key);
      best_star = Some(star);
    }
  }

  Some((best_key?, best_star?))
}

fn is_relative_specifier(specifier: &str) -> bool {
  specifier.starts_with("./") || specifier.starts_with("../")
}

fn subpath_needs_normalization(subpath: &str) -> bool {
  if subpath == "." || subpath == ".." {
    return true;
  }
  let bytes = subpath.as_bytes();
  if bytes.starts_with(b"./") || bytes.starts_with(b"../") {
    return true;
  }
  if bytes.len() >= 2 && bytes[bytes.len() - 2] == b'/' && bytes[bytes.len() - 1] == b'.' {
    return true;
  }
  if bytes.len() >= 3
    && bytes[bytes.len() - 3] == b'/'
    && bytes[bytes.len() - 2] == b'.'
    && bytes[bytes.len() - 1] == b'.'
  {
    return true;
  }

  let mut prev3 = 0u8;
  let mut prev2 = 0u8;
  let mut prev = 0u8;
  for &b in bytes {
    if b == b'\\' {
      return true;
    }
    if prev == b'/' && b == b'/' {
      return true;
    }
    if prev2 == b'/' && prev == b'.' && b == b'/' {
      return true;
    }
    if prev3 == b'/' && prev2 == b'.' && prev == b'.' && b == b'/' {
      return true;
    }
    prev3 = prev2;
    prev2 = prev;
    prev = b;
  }

  false
}

fn is_source_root(name: &str) -> bool {
  match name.as_bytes().last().copied() {
    Some(b's') => {
      name.ends_with(".ts")
        || name.ends_with(".d.ts")
        || name.ends_with(".js")
        || name.ends_with(".mjs")
        || name.ends_with(".cjs")
        || name.ends_with(".mts")
        || name.ends_with(".cts")
        || name.ends_with(".d.mts")
        || name.ends_with(".d.cts")
    }
    Some(b'x') => name.ends_with(".tsx") || name.ends_with(".jsx"),
    _ => false,
  }
}

fn is_drive_root(dir: &str) -> bool {
  let bytes = dir.as_bytes();
  bytes.len() == 3 && bytes[1] == b':' && bytes[2] == b'/' && bytes[0].is_ascii_alphabetic()
}

fn starts_with_drive_letter(path: &str) -> bool {
  let bytes = path.as_bytes();
  bytes.len() >= 3
    && bytes[0].is_ascii_alphabetic()
    && bytes[1] == b':'
    && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn virtual_parent_dir_str(path: &str) -> &str {
  if path == "/" || is_drive_root(path) {
    return path;
  }

  let trimmed = path.trim_end_matches('/');
  if trimmed == "/" || is_drive_root(trimmed) {
    return trimmed;
  }

  let Some(idx) = trimmed.rfind('/') else {
    return "/";
  };

  if idx == 0 {
    return "/";
  }

  let bytes = trimmed.as_bytes();
  if idx == 2 && bytes.get(1) == Some(&b':') && bytes.get(2) == Some(&b'/') {
    return &trimmed[..3];
  }

  &trimmed[..idx]
}

fn virtual_join(base: &str, segment: &str) -> String {
  if base == "/" {
    let mut joined = String::with_capacity(1 + segment.len());
    joined.push('/');
    joined.push_str(segment);
    joined
  } else {
    let mut joined = String::with_capacity(base.len() + 1 + segment.len());
    joined.push_str(base);
    if !base.ends_with('/') {
      joined.push('/');
    }
    joined.push_str(segment);
    joined
  }
}

fn virtual_join_into(out: &mut String, base: &str, segment: &str) {
  out.clear();
  out.reserve(base.len() + segment.len() + 1);
  if base == "/" {
    out.push('/');
    out.push_str(segment);
  } else {
    out.push_str(base);
    if !base.ends_with('/') {
      out.push('/');
    }
    out.push_str(segment);
  }
}

fn virtual_join3_into(out: &mut String, base: &str, segment: &str, tail: &str) {
  out.clear();
  out.reserve(base.len() + segment.len() + tail.len() + 2);
  out.push_str(base);
  if base != "/" && !base.ends_with('/') {
    out.push('/');
  }
  out.push_str(segment);
  out.push('/');
  out.push_str(tail);
}

fn types_fallback_specifier<'a>(specifier: &'a str) -> Option<Cow<'a, str>> {
  let (package, rest) = split_package_name(specifier)?;
  if package.starts_with("@types/") {
    return None;
  }

  if let Some(stripped) = package.strip_prefix('@') {
    let (scope, name) = stripped.split_once('/')?;
    let mut mapped = String::with_capacity(scope.len() + 2 + name.len() + rest.len());
    mapped.push_str(scope);
    mapped.push_str("__");
    mapped.push_str(name);
    mapped.push_str(rest);
    Some(Cow::Owned(mapped))
  } else {
    Some(Cow::Borrowed(specifier))
  }
}

fn split_package_name(specifier: &str) -> Option<(&str, &str)> {
  if specifier.is_empty() {
    return None;
  }

  if let Some(stripped) = specifier.strip_prefix('@') {
    let Some((scope, rest)) = stripped.split_once('/') else {
      return None;
    };
    let Some((name, _trailing)) = rest.split_once('/') else {
      let package_len = 1 + scope.len() + 1 + rest.len();
      return Some((&specifier[..package_len], ""));
    };

    let package_len = 1 + scope.len() + 1 + name.len();
    Some((&specifier[..package_len], &specifier[package_len..]))
  } else if let Some((package, _trailing)) = specifier.split_once('/') {
    let package_len = package.len();
    Some((&specifier[..package_len], &specifier[package_len..]))
  } else {
    Some((specifier, ""))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeMap;

  #[derive(Clone, Default)]
  struct FakeFs {
    files: BTreeMap<PathBuf, String>,
  }

  impl FakeFs {
    fn insert(&mut self, path: &str, contents: &str) {
      self.files.insert(PathBuf::from(path), contents.to_string());
    }
  }

  impl ResolveFs for FakeFs {
    fn is_file(&self, path: &Path) -> bool {
      self.files.contains_key(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
      self.files.keys().any(|p| p.starts_with(path) && p != path)
    }

    fn read_to_string(&self, path: &Path) -> Option<String> {
      self.files.get(path).cloned()
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
      Some(path.to_path_buf())
    }
  }

  #[test]
  fn resolves_package_json_types_entrypoints() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "types": "./dist/index.d.ts" }"#,
    );
    fs.insert("/node_modules/pkg/dist/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg")
      .expect("pkg should resolve");
    assert_eq!(resolved, PathBuf::from("/node_modules/pkg/dist/index.d.ts"));
  }

  #[test]
  fn classic_mode_does_not_resolve_bare_specifiers() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/node_modules/pkg/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: false,
        package_imports: false,
        ..ResolveOptions::default()
      },
    );
    assert!(
      resolver.resolve(Path::new("/src/app.ts"), "pkg").is_none(),
      "Classic module resolution should not search node_modules for bare specifiers"
    );
  }

  #[test]
  fn classic_mode_does_not_resolve_directory_indexes() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/src/dir/index.ts", "export const x = 1;\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: false,
        package_imports: false,
        ..ResolveOptions::default()
      },
    );
    assert!(
      resolver.resolve(Path::new("/src/app.ts"), "./dir").is_none(),
      "Classic module resolution should not resolve directory specifiers via index.* probing"
    );
  }

  #[test]
  fn classic_mode_does_not_consult_package_json_for_directory_specifiers() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/src/dir/package.json", r#"{ "types": "./dist/index.d.ts" }"#);
    fs.insert("/src/dir/dist/index.d.ts", "export const x: number;\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: false,
        package_imports: false,
        ..ResolveOptions::default()
      },
    );
    assert!(
      resolver.resolve(Path::new("/src/app.ts"), "./dir").is_none(),
      "Classic module resolution should not resolve directory specifiers via package.json"
    );
  }

  #[test]
  fn node_mode_resolves_directory_indexes() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/src/dir/index.ts", "export const x = 1;\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "./dir")
      .expect("./dir should resolve under node resolution");
    assert_eq!(resolved, PathBuf::from("/src/dir/index.ts"));
  }

  #[test]
  fn resolves_package_json_exports_types_entrypoints() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "exports": { ".": { "types": "./dist/index.d.ts" } } }"#,
    );
    fs.insert("/node_modules/pkg/dist/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg")
      .expect("pkg should resolve");
    assert_eq!(resolved, PathBuf::from("/node_modules/pkg/dist/index.d.ts"));
  }

  #[test]
  fn resolves_exports_subpath_mapping() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "exports": { "./subpath": { "types": "./dist/sub.d.ts" } } }"#,
    );
    fs.insert("/node_modules/pkg/dist/sub.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg/subpath")
      .expect("subpath should resolve");
    assert_eq!(resolved, PathBuf::from("/node_modules/pkg/dist/sub.d.ts"));
  }

  #[test]
  fn resolves_package_imports_map() {
    let mut fs = FakeFs::default();
    fs.insert("/project/src/app.ts", "");
    fs.insert(
      "/project/package.json",
      r##"{ "imports": { "#dep": "./src/dep.ts" } }"##,
    );
    fs.insert("/project/src/dep.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        package_imports: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/project/src/app.ts"), "#dep")
      .expect("imports entry should resolve");
    assert_eq!(resolved, PathBuf::from("/project/src/dep.ts"));
  }

  #[test]
  fn falls_back_to_at_types_packages() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/node_modules/@types/pkg/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg")
      .expect("@types fallback should resolve");
    assert_eq!(
      resolved,
      PathBuf::from("/node_modules/@types/pkg/index.d.ts")
    );
  }

  #[test]
  fn normalizes_windows_paths_during_resolution() {
    let mut fs = FakeFs::default();
    fs.insert("c:/project/src/app.ts", "");
    fs.insert("c:/project/node_modules/pkg/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new(r"C:\project\src\app.ts"), "pkg")
      .expect("pkg should resolve under c:/project");
    assert_eq!(
      resolved,
      PathBuf::from("c:/project/node_modules/pkg/index.d.ts")
    );
  }

  #[test]
  fn selects_exports_conditions_based_on_module_resolution_mode() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "exports": { ".": { "import": { "types": "./dist/index.d.mts" }, "require": { "types": "./dist/index.d.cts" } } } }"#,
    );
    fs.insert("/node_modules/pkg/dist/index.d.mts", "export {};\n");
    fs.insert("/node_modules/pkg/dist/index.d.cts", "export {};\n");

    let node16 = Resolver::with_fs(
      fs.clone(),
      ResolveOptions {
        node_modules: true,
        module_resolution: ModuleResolutionMode::Node16,
        ..ResolveOptions::default()
      },
    );
    let nodenext = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        module_resolution: ModuleResolutionMode::NodeNext,
        ..ResolveOptions::default()
      },
    );

    let resolved_node16 = node16
      .resolve(Path::new("/src/app.ts"), "pkg")
      .expect("node16 should resolve");
    let resolved_nodenext = nodenext
      .resolve(Path::new("/src/app.ts"), "pkg")
      .expect("nodenext should resolve");

    assert_eq!(resolved_node16, PathBuf::from("/node_modules/pkg/dist/index.d.cts"));
    assert_eq!(
      resolved_nodenext,
      PathBuf::from("/node_modules/pkg/dist/index.d.mts")
    );
  }

  #[test]
  fn selects_exports_conditions_based_on_import_vs_require() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "exports": { ".": { "import": { "types": "./dist/import.d.ts" }, "require": { "types": "./dist/require.d.ts" } } } }"#,
    );
    fs.insert("/node_modules/pkg/dist/import.d.ts", "export {};\n");
    fs.insert("/node_modules/pkg/dist/require.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        module_resolution: ModuleResolutionMode::Node16,
        ..ResolveOptions::default()
      },
    );

    let resolved_import = resolver
      .resolve_with_kind(Path::new("/src/app.ts"), "pkg", ResolutionKind::Import)
      .expect("import should resolve");
    let resolved_require = resolver
      .resolve_with_kind(Path::new("/src/app.ts"), "pkg", ResolutionKind::Require)
      .expect("require should resolve");

    assert_eq!(resolved_import, PathBuf::from("/node_modules/pkg/dist/import.d.ts"));
    assert_eq!(
      resolved_require,
      PathBuf::from("/node_modules/pkg/dist/require.d.ts")
    );
  }

  #[test]
  fn resolves_types_versions_mappings() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "typesVersions": { ">=5.1": { "*": ["ts5.1/*"] }, ">=5.0": { "*": ["ts5.0/*"] } } }"#,
    );
    fs.insert("/node_modules/pkg/ts5.0/subpath.d.ts", "export {};\n");
    fs.insert("/node_modules/pkg/ts5.1/subpath.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg/subpath")
      .expect("typesVersions subpath should resolve");
    assert_eq!(resolved, PathBuf::from("/node_modules/pkg/ts5.1/subpath.d.ts"));
  }

  #[test]
  fn selects_first_matching_types_versions_range() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/pkg/package.json",
      r#"{ "typesVersions": { ">=4.0": { "*": ["ts4/*"] }, ">=5.0": { "*": ["ts5/*"] } } }"#,
    );
    fs.insert("/node_modules/pkg/ts4/subpath.d.ts", "export {};\n");
    fs.insert("/node_modules/pkg/ts5/subpath.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "pkg/subpath")
      .expect("typesVersions subpath should resolve");
    assert_eq!(resolved, PathBuf::from("/node_modules/pkg/ts4/subpath.d.ts"));
  }

  #[test]
  fn falls_back_to_at_types_for_scoped_packages() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert(
      "/node_modules/@types/scope__pkg/index.d.ts",
      "export {};\n",
    );

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/src/app.ts"), "@scope/pkg")
      .expect("@types fallback should resolve for scoped package");
    assert_eq!(
      resolved,
      PathBuf::from("/node_modules/@types/scope__pkg/index.d.ts")
    );
  }

  #[test]
  fn classic_does_not_search_node_modules() {
    let mut fs = FakeFs::default();
    fs.insert("/src/app.ts", "");
    fs.insert("/src/node_modules/pkg/index.d.ts", "export {};\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        node_modules: true,
        package_imports: true,
        module_resolution: ModuleResolutionMode::Classic,
        ..ResolveOptions::default()
      },
    );
    assert!(
      resolver.resolve(Path::new("/src/app.ts"), "pkg").is_none(),
      "classic resolution should not resolve packages from node_modules"
    );
  }

  #[test]
  fn classic_searches_up_parent_directories() {
    let mut fs = FakeFs::default();
    fs.insert("/project/nested/app.ts", "");
    fs.insert("/project/utils.ts", "export const value = 1;\n");

    let resolver = Resolver::with_fs(
      fs,
      ResolveOptions {
        module_resolution: ModuleResolutionMode::Classic,
        ..ResolveOptions::default()
      },
    );
    let resolved = resolver
      .resolve(Path::new("/project/nested/app.ts"), "utils")
      .expect("classic should resolve utils.ts from a parent directory");
    assert_eq!(resolved, PathBuf::from("/project/utils.ts"));
  }
}
