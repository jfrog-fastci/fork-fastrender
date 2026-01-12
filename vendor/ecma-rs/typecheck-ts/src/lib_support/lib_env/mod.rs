use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use diagnostics::{Diagnostic, FileId, Span, TextRange};

use super::{CompilerOptions, FileKind, LibFile, LibSet};
use crate::codes;

pub(crate) mod prepared;

/// Loaded libraries for a particular set of options.
#[derive(Clone, Debug)]
pub struct LoadedLibs {
  pub lib_set: LibSet,
  pub files: Vec<LibFile>,
  pub diagnostics: Vec<Diagnostic>,
}

impl LoadedLibs {
  pub fn empty() -> Self {
    LoadedLibs {
      lib_set: LibSet::empty(),
      files: Vec::new(),
      diagnostics: Vec::new(),
    }
  }
}

/// Simple manager that caches bundled libs for a given option set and tracks loads.
#[derive(Debug, Default)]
pub struct LibManager {
  cache: Mutex<Option<(CompilerOptions, LoadedLibs)>>,
  load_count: AtomicUsize,
}

impl LibManager {
  pub fn new() -> Self {
    LibManager {
      cache: Mutex::new(None),
      load_count: AtomicUsize::new(0),
    }
  }

  /// How many times bundled libs were recomputed (useful for invalidation tests).
  pub fn load_count(&self) -> usize {
    self.load_count.load(Ordering::SeqCst)
  }

  /// Return libs appropriate for the provided compiler options. If the options change,
  /// cached results are invalidated and libs are reloaded.
  pub fn bundled_libs(&self, options: &CompilerOptions) -> LoadedLibs {
    let mut cache = self.cache.lock().unwrap();
    if let Some((ref cached_opts, ref libs)) = *cache {
      if cached_opts == options {
        return libs.clone();
      }
    }

    let lib_set = LibSet::for_options(options);
    let BundledLoadResult { files, diagnostics } = load_bundled(&lib_set);
    let result = LoadedLibs {
      lib_set: lib_set.clone(),
      files,
      diagnostics,
    };
    *cache = Some((options.clone(), result.clone()));
    self.load_count.fetch_add(1, Ordering::SeqCst);
    result
  }
}

#[derive(Debug)]
struct BundledLoadResult {
  files: Vec<LibFile>,
  diagnostics: Vec<Diagnostic>,
}

fn load_bundled(lib_set: &LibSet) -> BundledLoadResult {
  #[cfg(feature = "bundled-libs")]
  {
    bundled::load_bundled(lib_set)
  }

  #[cfg(not(feature = "bundled-libs"))]
  {
    if lib_set.libs().is_empty() {
      return BundledLoadResult {
        files: Vec::new(),
        diagnostics: Vec::new(),
      };
    }

    BundledLoadResult {
      files: vec![fallback_core_globals_lib()],
      diagnostics: Vec::new(),
    }
  }
}

pub fn bundled_lib_file(name: super::LibName) -> Option<LibFile> {
  #[cfg(feature = "bundled-libs")]
  {
    bundled::lib_file(name)
  }

  #[cfg(not(feature = "bundled-libs"))]
  {
    let _ = name;
    None
  }
}

/// The TypeScript version that the bundled `lib.*.d.ts` files were sourced from.
///
/// When the `bundled-libs` feature is disabled, no `.d.ts` libs are embedded
/// and this returns `None`.
pub fn bundled_typescript_version() -> Option<&'static str> {
  #[cfg(feature = "bundled-libs")]
  {
    Some(bundled::typescript_version())
  }

  #[cfg(not(feature = "bundled-libs"))]
  {
    None
  }
}

#[cfg(not(feature = "bundled-libs"))]
const FALLBACK_CORE_GLOBAL_TYPES: &str = r#"
interface Array<T> {}
interface Boolean {}
interface Function {}
interface IArguments {}
interface Number {}
interface Object {}
interface RegExp {}
interface String {}
interface Symbol {}
interface SymbolConstructor {
  readonly dispose: unique symbol;
  readonly asyncDispose: unique symbol;
}

declare var Array: any;
declare var Boolean: any;
declare var Function: any;
declare var Number: any;
declare var Object: any;
declare var RegExp: any;
declare var String: any;
declare var Symbol: SymbolConstructor;

type Uppercase<S extends string> = intrinsic;
type Lowercase<S extends string> = intrinsic;
type Capitalize<S extends string> = intrinsic;
type Uncapitalize<S extends string> = intrinsic;
type NoInfer<T> = intrinsic;
type BuiltinIteratorReturn = intrinsic;

interface PromiseLike<T> {
  then<TResult1 = T, TResult2 = never>(
    onfulfilled?: ((value: T) => TResult1 | PromiseLike<TResult1>) | null,
    onrejected?: ((reason: any) => TResult2 | PromiseLike<TResult2>) | null,
  ): PromiseLike<TResult1 | TResult2>;
}

interface Disposable {
  [Symbol.dispose](): void;
}

interface AsyncDisposable {
  [Symbol.asyncDispose](): PromiseLike<void>;
}
"#;

#[cfg(not(feature = "bundled-libs"))]
fn fallback_core_globals_lib() -> LibFile {
  use std::sync::Arc;

  use crate::FileKey;

  let key = FileKey::new("lib:core_globals.d.ts");
  let text = prepared::bundled_lib_text_arc(&key, FileKind::Dts, FALLBACK_CORE_GLOBAL_TYPES);
  LibFile {
    key,
    name: Arc::from("core_globals.d.ts"),
    kind: FileKind::Dts,
    text,
  }
}

pub fn bundled_lib_file_by_option_name(name: &str) -> Option<LibFile> {
  #[cfg(feature = "bundled-libs")]
  {
    bundled::lib_file_by_option_name(name)
  }

  #[cfg(not(feature = "bundled-libs"))]
  {
    let _ = name;
    None
  }
}

#[cfg(feature = "bundled-libs")]
mod bundled {
  use std::collections::{BTreeSet, VecDeque};
  use std::sync::Arc;

  use diagnostics::{Diagnostic, FileId, Span, TextRange};

  use super::super::{FileKind, LibFile, LibName, LibSet};
  use crate::codes;
  use crate::FileKey;

  const LIB_OPTION_SPAN: Span = Span::new(FileId(u32::MAX), TextRange::new(0, 0));

  pub(super) fn lib_file(name: LibName) -> Option<LibFile> {
    let canonical = LibName::parse(name.as_str())?;
    lib_file_by_filename(&canonical.file_name())
  }

  pub(super) fn lib_file_by_option_name(option_name: &str) -> Option<LibFile> {
    let option_name = option_name.trim();
    if option_name.is_empty() {
      return None;
    }
    let filename = format!("lib.{}.d.ts", option_name.to_ascii_lowercase());
    lib_file_by_filename(&filename)
  }

  fn lib_file_by_filename(filename: &str) -> Option<LibFile> {
    bundled_lib_text(filename).map(|text| {
      let key = FileKey::new(format!("lib:{filename}"));
      let text = super::prepared::bundled_lib_text_arc(&key, FileKind::Dts, text);
      LibFile {
        key,
        name: Arc::from(filename),
        kind: FileKind::Dts,
        text,
      }
    })
  }

  pub fn load_bundled(lib_set: &LibSet) -> super::BundledLoadResult {
    let mut required: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut invalid_libs: BTreeSet<String> = BTreeSet::new();

    for name in lib_set.libs() {
      let Some(canonical) = LibName::parse(name.as_str()) else {
        invalid_libs.insert(name.as_str().trim().to_string());
        continue;
      };
      let option_name = canonical.as_str().to_string();
      let file = canonical.file_name();
      if bundled_lib_text(&file).is_none() {
        invalid_libs.insert(option_name);
        continue;
      }
      if required.insert(file.clone()) {
        queue.push_back(file);
      }
    }

    while let Some(file) = queue.pop_front() {
      let Some(text) = bundled_lib_text(&file) else {
        invalid_libs.insert(file.clone());
        continue;
      };
      for dep in referenced_libs(text).into_iter() {
        if bundled_lib_text(&dep).is_none() {
          invalid_libs.insert(dep.clone());
          continue;
        }
        if required.insert(dep.clone()) {
          queue.push_back(dep);
        }
      }
    }

    let files: Vec<LibFile> = required
      .into_iter()
      .filter_map(|filename| lib_file_by_filename(&filename))
      .collect();

    let diagnostics: Vec<Diagnostic> = invalid_libs
      .into_iter()
      .filter(|name| !name.trim().is_empty())
      .map(|name| {
        codes::INVALID_LIB_OPTION.error(
          format!(
            "Invalid value for '--lib': '{}'. The '--lib' option expects known TypeScript library names.",
            name
          ),
          LIB_OPTION_SPAN,
        )
      })
      .collect();

    super::BundledLoadResult { files, diagnostics }
  }

  mod generated {
    include!(concat!(env!("OUT_DIR"), "/typescript_libs_generated.rs"));
  }

  pub(super) fn typescript_version() -> &'static str {
    generated::TYPESCRIPT_VERSION
  }

  // Ensure the build-script generated `TYPESCRIPT_VERSION` constant is treated as used
  // so it does not trigger `dead_code` warnings in downstream builds.
  const _: &str = generated::TYPESCRIPT_VERSION;

  pub(super) fn bundled_lib_text(filename: &str) -> Option<&'static str> {
    match generated::LIBS.binary_search_by(|(name, _)| name.cmp(&filename)) {
      Ok(idx) => Some(generated::LIBS[idx].1),
      Err(_) => None,
    }
  }

  fn referenced_libs(text: &str) -> Vec<String> {
    fn attr_value<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
      let mut offset = 0;
      while let Some(found) = line[offset..].find(needle) {
        let start = offset + found;
        // Avoid matching `no-default-lib="true"` as `lib="true"` by requiring
        // the attribute name be preceded by whitespace.
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
        out.push(format!("lib.{lib_name}.d.ts"));
      }

      if let Some(path) = attr_value(line, "path=\"") {
        let filename = path.rsplit('/').next().unwrap_or(path);
        if filename.starts_with("lib.") && filename.ends_with(".d.ts") {
          out.push(filename.to_string());
        }
      }
    }
    out
  }
}

/// Result of validating a set of libraries.
#[derive(Clone, Debug)]
pub struct LibValidationResult {
  /// Libraries that passed validation, paired with their allocated [`FileId`].
  pub libs: Vec<(LibFile, FileId)>,
  /// Diagnostics produced while validating the libraries.
  pub diagnostics: Vec<Diagnostic>,
}

impl LibValidationResult {
  /// Empty validation result used when no libs are available.
  pub fn empty() -> Self {
    LibValidationResult {
      libs: Vec::new(),
      diagnostics: Vec::new(),
    }
  }
}

/// Lib files collected from both the host and the bundled TypeScript distribution.
#[derive(Clone, Debug)]
pub struct CollectedLibs {
  pub files: Vec<LibFile>,
  pub diagnostics: Vec<Diagnostic>,
}

/// Merge host-provided libs with bundled libs selected from [`CompilerOptions`].
pub fn collect_libs(
  options: &CompilerOptions,
  mut host_libs: Vec<LibFile>,
  lib_manager: &LibManager,
) -> CollectedLibs {
  let bundled = lib_manager.bundled_libs(options);
  host_libs.extend(bundled.files);
  CollectedLibs {
    files: host_libs,
    diagnostics: bundled.diagnostics,
  }
}

/// Filter out non-`.d.ts` libraries, emitting diagnostics for any ignored entries
/// and for the absence of any valid libs.
pub fn validate_libs(
  mut libs: Vec<LibFile>,
  mut file_id_for: impl FnMut(&LibFile) -> FileId,
) -> LibValidationResult {
  if libs.is_empty() {
    return LibValidationResult {
      libs: Vec::new(),
      diagnostics: vec![codes::NO_LIBS_LOADED.error(
        "No library files were loaded. Provide libs via the host or enable the bundled-libs feature / disable no_default_lib.",
        Span::new(FileId(u32::MAX), TextRange::new(0, 0)),
      )],
    };
  }

  libs.sort_by(|a, b| (a.name.as_ref(), a.key.as_str()).cmp(&(b.name.as_ref(), b.key.as_str())));

  let mut diagnostics = Vec::new();
  let mut filtered = Vec::new();
  for lib in libs {
    let file_id = file_id_for(&lib);
    let is_dts = lib.kind == FileKind::Dts || lib.name.ends_with(".d.ts");
    if !is_dts {
      diagnostics.push(codes::NON_DTS_LIB.warning(
        format!(
          "Library '{}' is not a .d.ts file; it will be ignored for global declarations.",
          lib.name
        ),
        Span::new(file_id, TextRange::new(0, 0)),
      ));
      continue;
    }
    filtered.push((lib, file_id));
  }

  if filtered.is_empty() {
    diagnostics.push(codes::NO_LIBS_LOADED.error(
      "No library files were loaded. Provide libs via the host or enable the bundled-libs feature / disable no_default_lib.",
      Span::new(FileId(u32::MAX), TextRange::new(0, 0)),
    ));
  }

  LibValidationResult {
    libs: filtered,
    diagnostics,
  }
}
