use super::*;

impl Program {
  /// Create a new program from a host and root file list.
  pub fn new(host: impl Host, roots: Vec<FileKey>) -> Program {
    Program::with_lib_manager(host, roots, Arc::new(LibManager::new()))
  }

  /// Root files configured for this program.
  ///
  /// This is the (sorted, deduplicated) `roots` list passed to [`Program::new`]
  /// / [`Program::with_lib_manager`]. It does not include any implicit lib files
  /// pulled in via compiler options.
  pub fn roots(&self) -> &[FileKey] {
    &self.roots
  }

  /// Create a new program with a provided lib manager (useful for observing invalidation in tests).
  pub fn with_lib_manager(
    host: impl Host,
    mut roots: Vec<FileKey>,
    lib_manager: Arc<LibManager>,
  ) -> Program {
    let host: Arc<dyn Host> = Arc::new(host);
    let query_stats = QueryStatsCollector::default();
    let cancelled = Arc::new(AtomicBool::new(false));
    roots.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    roots.dedup_by(|a, b| a.as_str() == b.as_str());
    let program = Program {
      host: Arc::clone(&host),
      roots,
      cancelled: Arc::clone(&cancelled),
      state: RwLock::new(ProgramState::new(
        Arc::clone(&host),
        lib_manager,
        query_stats.clone(),
        Arc::clone(&cancelled),
      )),
      query_stats,
    };
    {
      let mut state = program.lock_state();
      for key in program.roots.iter().cloned() {
        state.intern_file_key(key, FileOrigin::Source);
      }
    }
    program
  }

  /// Compiler options used by this program.
  pub fn compiler_options(&self) -> CompilerOptions {
    match self.with_analyzed_state(|state| Ok(state.compiler_options.clone())) {
      Ok(opts) => opts,
      Err(fatal) => {
        self.record_fatal(fatal);
        CompilerOptions::default()
      }
    }
  }

  /// Override the compiler options for subsequent queries.
  pub fn set_compiler_options(&mut self, options: CompilerOptions) {
    {
      let mut state = self.lock_state();
      state.invalidate_all_analysis();
      {
        let mut db = state.typecheck_db.lock();
        db.clear_file_origins();
        db.clear_body_results();
        db.set_compiler_options(options.clone());
      }
      state.compiler_options = options.clone();
      state.compiler_options_override = Some(options.clone());
      state.checker_caches = CheckerCaches::new(options.cache.clone());
      *state.cache_stats.lock() = CheckerCacheStats::default();
      state.store = tti::TypeStore::with_options((&options).into());
      let store = Arc::clone(&state.store);
      state
        .typecheck_db
        .lock()
        .set_type_store(crate::db::types::SharedTypeStore(store));
    }
  }

  /// Override the text for a specific file and invalidate cached results.
  pub fn set_file_text(&mut self, file: FileId, text: Arc<str>) {
    {
      let mut state = self.lock_state();
      let Some(key) = state.file_key_for_id(file) else {
        return;
      };

      state.file_overrides.insert(key.clone(), Arc::clone(&text));
      let mut db = state.typecheck_db.lock();
      if db::Db::file_input(&*db, file).is_some() {
        db.set_file_text(file, text);
      }
      drop(db);
      state.invalidate_on_file_text_change(file);
    }
  }

  /// Resolve a file key to its internal [`FileId`], if loaded.
  pub fn file_id(&self, key: &FileKey) -> Option<FileId> {
    match self.with_analyzed_state(|state| Ok(state.file_id_for_key(key))) {
      Ok(id) => id,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  /// All [`FileId`]s associated with a [`FileKey`], preferring source-origin IDs first.
  pub fn file_ids_for_key(&self, key: &FileKey) -> Vec<FileId> {
    self
      .with_analyzed_state(|state| Ok(state.file_ids_for_key(key)))
      .unwrap_or_default()
  }

  /// Resolve a loaded [`FileId`] back to its [`FileKey`], if available.
  pub fn file_key(&self, file: FileId) -> Option<FileKey> {
    let state = self.lock_state();
    state.file_key_for_id(file)
  }

  /// Text contents for a loaded file, if available from the host.
  pub fn file_text(&self, file: FileId) -> Option<Arc<str>> {
    match self.with_analyzed_state(|state| Ok(state.load_text(file, &self.host).ok())) {
      Ok(text) => text,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  /// Cached `hir-js` lowering for a loaded file, if available.
  ///
  /// This exposes the lowering computed during analysis so downstream tools can
  /// share HIR IDs (`BodyId`/`ExprId`) with the type checker without having to
  /// re-lower the same parsed AST.
  pub fn hir_lowered(&self, file: FileId) -> Option<Arc<hir_js::LowerResult>> {
    match self.with_analyzed_state(|state| Ok(state.hir_lowered.get(&file).cloned())) {
      Ok(lowered) => lowered,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  /// All known file IDs in this program.
  pub fn files(&self) -> Vec<FileId> {
    self
      .with_analyzed_state(|state| {
        let mut files: Vec<FileId> = state.files.keys().copied().collect();
        files.sort_by_key(|id| id.0);
        Ok(files)
      })
      .unwrap_or_default()
  }

  /// All files reachable from the configured roots.
  pub fn reachable_files(&self) -> Vec<FileId> {
    self
      .with_analyzed_state(|state| {
        let mut files: Vec<FileId> = if state.snapshot_loaded {
         use std::collections::{BTreeMap, VecDeque};

           let mut edges: BTreeMap<FileId, Vec<FileId>> = BTreeMap::new();
           for (from, _specifier, resolved) in state.typecheck_db.lock().module_resolutions_snapshot() {
             let Some(resolved) = resolved else {
               continue;
             };
             edges.entry(from).or_default().push(resolved);
           }
          for deps in edges.values_mut() {
            deps.sort_by_key(|id| id.0);
            deps.dedup();
          }

          let mut queue: VecDeque<FileId> = state.root_ids.iter().copied().collect();
          let mut libs: Vec<FileId> = state.lib_file_ids.iter().copied().collect();
          libs.sort_by_key(|id| id.0);
          queue.extend(libs);

          let mut visited = BTreeMap::<FileId, ()>::new();
          while let Some(file) = queue.pop_front() {
            if visited.contains_key(&file) {
              continue;
            }
            visited.insert(file, ());
            if let Some(deps) = edges.get(&file) {
              for dep in deps {
                queue.push_back(*dep);
              }
            }
          }

          visited
            .keys()
            .copied()
            .filter(|file| !state.lib_file_ids.contains(file))
            .collect()
         } else {
           state
             .typecheck_db
             .lock()
             .reachable_files()
             .iter()
             .copied()
             .filter(|file| !state.lib_file_ids.contains(file))
            .collect()
        };
        files.sort_by_key(|id| id.0);
        Ok(files)
      })
      .unwrap_or_default()
  }

  /// Resolve a module specifier relative to a file, returning the file-backed [`FileId`].
  ///
  /// This uses the module resolution edges recorded in the program's internal salsa database
  /// (the same results observed by the checker) instead of calling [`Host::resolve`](crate::Host).
  ///
  /// Returns `None` when the module specifier is unresolved, refers to an ambient module, or
  /// otherwise does not map to a file in the program.
  pub fn resolve_module(&self, from: FileId, specifier: &str) -> Option<FileId> {
    match self.with_analyzed_state(|state| {
      let db = state.typecheck_db.lock();
      Ok(db::queries::module_resolve_ref(&*db, from, specifier))
    }) {
      Ok(resolved) => resolved,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  /// Deterministically list file-backed module dependencies recorded for a file.
  ///
  /// Returned entries are ordered by module specifier (stable across runs) and include only
  /// specifiers that resolved to a file in the program (ambient modules are excluded).
  pub fn resolved_module_deps(&self, from: FileId) -> Vec<(String, FileId)> {
    match self.with_analyzed_state(|state| {
      Ok(
        state
          .typecheck_db
          .lock()
          .module_resolutions_snapshot_for_file(from)
          .into_iter()
          .filter_map(|(specifier, resolved)| resolved.map(|file| (specifier, file)))
          .collect(),
      )
    }) {
      Ok(deps) => deps,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  /// All files that directly depend on `file` via file-backed module imports.
  ///
  /// The result is deterministic and sorted by [`FileId`]. Ambient module specifiers are excluded
  /// (only edges that resolved to a file are considered).
  pub fn reverse_module_deps(&self, file: FileId) -> Vec<FileId> {
    match self.with_analyzed_state(|state| {
      let db = state.typecheck_db.lock().clone();
      Ok(db.module_reverse_deps(file).iter().copied().collect())
    }) {
      Ok(files) => files,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  /// Reverse dependency closure for `file`, including `file` itself.
  ///
  /// The result is deterministic and sorted by [`FileId`]. Ambient module specifiers are excluded
  /// (only edges that resolved to a file are considered).
  pub fn transitive_reverse_module_deps(&self, file: FileId) -> Vec<FileId> {
    match self.with_analyzed_state(|state| {
      let db = state.typecheck_db.lock().clone();
      Ok(db.module_transitive_reverse_deps(file).as_ref().clone())
    }) {
      Ok(files) => files,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }
}

impl Host for Program {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self.host.file_text(file)
  }

  fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey> {
    self.host.resolve(from, specifier)
  }

  fn compiler_options(&self) -> CompilerOptions {
    self.host.compiler_options()
  }

  fn lib_files(&self) -> Vec<LibFile> {
    self.host.lib_files()
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    self.host.file_kind(file)
  }
}

#[cfg(test)]
mod tests {
  use crate::{FileKey, MemoryHost, Program};

  #[test]
  fn resolve_module_returns_expected_file_ids() {
    let mut host = MemoryHost::new();
    let entry = FileKey::new("entry.ts");
    let dep = FileKey::new("dep.ts");
    host.insert(
      entry.clone(),
      r#"
import { value } from "./dep";
import { missing } from "./missing";
export const out = value;
"#,
    );
    host.insert(dep.clone(), "export const value = 1;");
    host.link(entry.clone(), "./dep", dep.clone());

    let program = Program::new(host, vec![entry.clone()]);
    let entry_id = program.file_id(&entry).unwrap();
    let dep_id = program.file_id(&dep).unwrap();

    assert_eq!(program.resolve_module(entry_id, "./dep"), Some(dep_id));
    assert_eq!(program.resolve_module(entry_id, "./missing"), None);
  }

  #[test]
  fn resolved_module_deps_are_deterministic_and_file_backed() {
    let mut host = MemoryHost::new();
    let entry = FileKey::new("entry.ts");
    let a = FileKey::new("a.ts");
    let b = FileKey::new("b.ts");
    let c = FileKey::new("c.ts");

    host.insert(
      entry.clone(),
      r#"
import "./b";
import "./a";
export * from "./c";
import "./missing";
"#,
    );
    host.insert(a.clone(), "export const a = 1;");
    host.insert(b.clone(), "export const b = 2;");
    host.insert(c.clone(), "export const c = 3;");

    host.link(entry.clone(), "./a", a.clone());
    host.link(entry.clone(), "./b", b.clone());
    host.link(entry.clone(), "./c", c.clone());

    let program = Program::new(host, vec![entry.clone()]);
    let entry_id = program.file_id(&entry).unwrap();
    let a_id = program.file_id(&a).unwrap();
    let b_id = program.file_id(&b).unwrap();
    let c_id = program.file_id(&c).unwrap();

    assert_eq!(
      program.resolved_module_deps(entry_id),
      vec![
        ("./a".to_string(), a_id),
        ("./b".to_string(), b_id),
        ("./c".to_string(), c_id),
      ]
    );
  }

  #[test]
  fn ambient_modules_are_not_file_backed() {
    let mut host = MemoryHost::new();
    let entry = FileKey::new("entry.ts");
    let ambient_decls = FileKey::new("ambient_decls.ts");
    host.insert(
      entry.clone(),
      r#"
import { x } from "ambient";
export const y = x;
"#,
    );
    host.insert(
      ambient_decls.clone(),
      r#"
declare module "ambient" {
  export const x: number;
}
"#,
    );

    let program = Program::new(host, vec![entry.clone(), ambient_decls.clone()]);
    assert!(
      program.check().is_empty(),
      "expected ambient module import to avoid unresolved-module diagnostics"
    );
    let entry_id = program.file_id(&entry).unwrap();
    assert_eq!(program.resolve_module(entry_id, "ambient"), None);
    assert!(program.resolved_module_deps(entry_id).is_empty());
  }
}
