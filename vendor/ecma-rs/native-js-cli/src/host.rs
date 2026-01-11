use crate::tsconfig;
use diagnostics::paths::normalize_fs_path;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::resolve::{canonicalize_path, NodeResolver};
use typecheck_ts::{FileKey, Host, HostError};

#[derive(Clone)]
pub struct ModuleResolver {
  pub resolver: NodeResolver,
  pub tsconfig: Option<TsconfigResolver>,
}

#[derive(Clone)]
pub struct TsconfigResolver {
  base_url: PathBuf,
  paths: Vec<TsconfigPathMapping>,
}

#[derive(Clone)]
struct TsconfigPathMapping {
  prefix: String,
  suffix: String,
  has_wildcard: bool,
  substitutions: Vec<String>,
}

#[derive(Clone)]
pub struct DiskHost {
  state: Arc<Mutex<DiskState>>,
  resolver: ModuleResolver,
  compiler_options: CompilerOptions,
  lib_files: Vec<LibFile>,
}

#[derive(Default, Clone)]
struct DiskState {
  path_to_key: BTreeMap<PathBuf, FileKey>,
  key_to_path: HashMap<FileKey, PathBuf>,
  key_to_kind: HashMap<FileKey, FileKind>,
  texts: HashMap<FileKey, Arc<str>>,
}

impl DiskHost {
  pub fn new(
    entries: &[PathBuf],
    resolver: ModuleResolver,
    compiler_options: CompilerOptions,
    lib_files: Vec<LibFile>,
  ) -> Result<(Self, Vec<FileKey>), String> {
    let mut state = DiskState::default();
    let mut roots = Vec::new();
    for entry in entries {
      let canonical = canonicalize_path(entry)
        .map_err(|err| format!("failed to read entry {}: {err}", entry.display()))?;
      let key = state.intern_path(canonical);
      roots.push(key);
    }

    Ok((
      DiskHost {
        state: Arc::new(Mutex::new(state)),
        resolver,
        compiler_options,
        lib_files,
      },
      roots,
    ))
  }

  pub fn key_for_path(&self, path: &Path) -> Option<FileKey> {
    let canonical = canonicalize_path(path).ok()?;
    let state = self.state.lock().unwrap();
    state.path_to_key.get(&canonical).cloned()
  }

  pub fn path_for_key(&self, key: &FileKey) -> Option<PathBuf> {
    let state = self.state.lock().unwrap();
    state.key_to_path.get(key).cloned()
  }
}

impl DiskState {
  fn intern_path(&mut self, path: PathBuf) -> FileKey {
    if let Some(key) = self.path_to_key.get(&path) {
      return key.clone();
    }
    // Use a stable, TypeScript-style virtual path as the file key so `FileId`
    // hashing is deterministic across platforms.
    let key = FileKey::new(normalize_fs_path(&path));
    let kind = file_kind_for(&path);
    self.path_to_key.insert(path.clone(), key.clone());
    self.key_to_path.insert(key.clone(), path);
    self.key_to_kind.insert(key.clone(), kind);
    key
  }
}

impl Host for DiskHost {
  fn file_text(&self, key: &FileKey) -> Result<Arc<str>, HostError> {
    let mut state = self.state.lock().unwrap();
    if let Some(text) = state.texts.get(key) {
      return Ok(text.clone());
    }
    let path = state
      .key_to_path
      .get(key)
      .cloned()
      .ok_or_else(|| HostError::new(format!("unknown file {key}")))?;
    let text = fs::read_to_string(&path)
      .map_err(|err| HostError::new(format!("failed to read {}: {err}", path.display())))?;
    let arc: Arc<str> = Arc::from(text);
    state.texts.insert(key.clone(), arc.clone());
    Ok(arc)
  }

  fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey> {
    let base = self.path_for_key(from).or_else(|| {
      let candidate = PathBuf::from(from.as_str());
      candidate.is_file().then_some(candidate)
    })?;

    let resolved = self.resolver.resolve(&base, specifier)?;
    let resolved = canonicalize_path(&resolved).unwrap_or(resolved);
    let mut state = self.state.lock().unwrap();
    Some(state.intern_path(resolved))
  }

  fn compiler_options(&self) -> CompilerOptions {
    self.compiler_options.clone()
  }

  fn lib_files(&self) -> Vec<LibFile> {
    self.lib_files.clone()
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    let state = self.state.lock().unwrap();
    state.key_to_kind.get(file).copied().unwrap_or(FileKind::Ts)
  }
}

impl ModuleResolver {
  pub fn resolve(&self, from: &Path, specifier: &str) -> Option<PathBuf> {
    if let Some(tsconfig) = self.tsconfig.as_ref() {
      if let Some(resolved) = tsconfig.resolve(from, specifier, &self.resolver) {
        return Some(resolved);
      }
    }
    self.resolver.resolve(from, specifier)
  }
}

impl TsconfigResolver {
  pub fn from_project(cfg: &tsconfig::ProjectConfig) -> Option<Self> {
    if cfg.base_url.is_none() && cfg.paths.is_empty() {
      return None;
    }
    let base_url = cfg.base_url.clone().unwrap_or_else(|| cfg.root_dir.clone());
    let mut paths = Vec::new();
    for (pattern, subs) in &cfg.paths {
      let (prefix, suffix, has_wildcard) = match pattern.split_once('*') {
        Some((pre, suf)) => (pre.to_string(), suf.to_string(), true),
        None => (pattern.clone(), String::new(), false),
      };
      paths.push(TsconfigPathMapping {
        prefix,
        suffix,
        has_wildcard,
        substitutions: subs.clone(),
      });
    }
    Some(TsconfigResolver { base_url, paths })
  }

  fn resolve(&self, from: &Path, specifier: &str, resolver: &NodeResolver) -> Option<PathBuf> {
    if is_relative_or_absolute_specifier(specifier) {
      return None;
    }

    if let Some(resolved) = self.resolve_via_paths(from, specifier, resolver) {
      return Some(resolved);
    }

    let candidate = self.base_url.join(specifier);
    resolver.resolve(from, candidate.to_string_lossy().as_ref())
  }

  fn resolve_via_paths(
    &self,
    from: &Path,
    specifier: &str,
    resolver: &NodeResolver,
  ) -> Option<PathBuf> {
    let mut best: Option<(&TsconfigPathMapping, String, (bool, usize, usize))> = None;
    for mapping in &self.paths {
      let Some(capture) = mapping.matches(specifier) else {
        continue;
      };
      let score = (
        !mapping.has_wildcard,
        mapping.prefix.len(),
        mapping.suffix.len(),
      );
      let replace = match best {
        Some((_, _, best_score)) => score > best_score,
        None => true,
      };
      if replace {
        best = Some((mapping, capture, score));
      }
    }

    let (mapping, capture, _) = best?;
    for sub in &mapping.substitutions {
      let substituted = if mapping.has_wildcard {
        sub.replace('*', &capture)
      } else {
        sub.clone()
      };
      let candidate = self.base_url.join(substituted);
      if let Some(resolved) = resolver.resolve(from, candidate.to_string_lossy().as_ref()) {
        return Some(resolved);
      }
    }
    None
  }
}

impl TsconfigPathMapping {
  fn matches(&self, specifier: &str) -> Option<String> {
    if self.has_wildcard {
      if !specifier.starts_with(&self.prefix) || !specifier.ends_with(&self.suffix) {
        return None;
      }
      let rest = specifier.strip_prefix(&self.prefix)?;
      let middle = rest.strip_suffix(&self.suffix)?;
      Some(middle.to_string())
    } else {
      (specifier == self.prefix).then(|| String::new())
    }
  }
}

fn is_relative_or_absolute_specifier(specifier: &str) -> bool {
  specifier.starts_with("./")
    || specifier.starts_with("../")
    || Path::new(specifier).is_absolute()
    || specifier.starts_with('/')
}

fn file_kind_for(path: &Path) -> FileKind {
  let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
  let name = name.to_ascii_lowercase();
  if name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts") {
    return FileKind::Dts;
  }
  if name.ends_with(".tsx") {
    return FileKind::Tsx;
  }
  if name.ends_with(".ts") || name.ends_with(".mts") || name.ends_with(".cts") {
    return FileKind::Ts;
  }
  if name.ends_with(".jsx") {
    return FileKind::Jsx;
  }
  if name.ends_with(".js") || name.ends_with(".mjs") || name.ends_with(".cjs") {
    return FileKind::Js;
  }

  FileKind::Ts
}

