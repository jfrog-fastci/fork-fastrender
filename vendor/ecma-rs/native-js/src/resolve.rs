use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use diagnostics::{FileId, TextRange};
use hir_js::{Body, ExprId, ExprKind, PatId, PatKind};
use typecheck_ts::{semantic_js::SymbolId, DefId, Program};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Identifier/binding resolver for a single `typecheck-ts::Program`.
///
/// This builds a per-file cache mapping identifier spans to `DefId`s by joining
/// `Program::debug_symbol_occurrences` with `Program::symbol_info(symbol).def`.
///
/// This avoids calling `Program::symbol_at(...)` in inner loops during codegen.
pub struct Resolver<'p> {
  program: &'p Program,
  files: Mutex<HashMap<FileId, Arc<FileMap>>>,
  #[cfg(test)]
  file_map_builds: AtomicUsize,
}

/// Per-file view over [`Resolver`].
///
/// `hir-js::Body` does not store its `FileId` (and top-level bodies use
/// `MISSING_DEF` as their owner), so callers must carry the file ID alongside
/// the body when resolving identifiers.
pub struct FileResolver<'r, 'p> {
  resolver: &'r Resolver<'p>,
  file: FileId,
}

/// Semantic identity for an identifier occurrence.
///
/// - `Def`: the identifier resolves to a `hir-js`/`typecheck-ts` `DefId` (used for
///   cross-module linking and globally addressable declarations).
/// - `Symbol`: a stable synthetic symbol id for locals that are not represented
///   in the global TypeScript semantics (e.g. block locals, parameters, nested
///   function declarations). These IDs are stable within a file and include the
///   scope discriminator so shadowed locals remain distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BindingId {
  Def(DefId),
  Symbol(SymbolId),
}

impl BindingId {
  pub fn as_def(self) -> Option<DefId> {
    match self {
      BindingId::Def(def) => Some(def),
      BindingId::Symbol(_) => None,
    }
  }
}

#[derive(Debug)]
struct FileMap {
  occurrences: Vec<ResolvedOccurrence>,
}

#[derive(Clone, Copy, Debug)]
struct ResolvedOccurrence {
  range: TextRange,
  binding: BindingId,
}

impl<'p> Resolver<'p> {
  pub fn new(program: &'p Program) -> Self {
    Self {
      program,
      files: Mutex::new(HashMap::new()),
      #[cfg(test)]
      file_map_builds: AtomicUsize::new(0),
    }
  }

  pub fn for_file(&self, file: FileId) -> FileResolver<'_, 'p> {
    FileResolver { resolver: self, file }
  }

  fn resolve_span(&self, file: FileId, span: TextRange) -> Option<BindingId> {
    let map = self.file_map(file);
    map.binding_at(span.start)
  }

  fn file_map(&self, file: FileId) -> Arc<FileMap> {
    let mut guard = self.files.lock().expect("resolver cache poisoned");
    if let Some(existing) = guard.get(&file) {
      return Arc::clone(existing);
    }

    #[cfg(test)]
    self.file_map_builds.fetch_add(1, Ordering::Relaxed);

    let mut occurrences: Vec<ResolvedOccurrence> = Vec::new();
    for (range, symbol) in self.program.debug_symbol_occurrences(file) {
      let Some(info) = self.program.symbol_info(symbol) else {
        continue;
      };
      let binding = if let Some(def) = info.def {
        BindingId::Def(def)
      } else {
        BindingId::Symbol(symbol)
      };
      occurrences.push(ResolvedOccurrence { range, binding });
    }

    // Keep deterministic order for deterministic binary search behaviour.
    occurrences.sort_by_key(|occ| {
      (
        occ.range.start,
        occ.range.end,
        match occ.binding {
          BindingId::Def(def) => def.0,
          BindingId::Symbol(symbol) => symbol.0,
        },
      )
    });

    let map = Arc::new(FileMap { occurrences });
    guard.insert(file, Arc::clone(&map));
    map
  }
}

impl<'r, 'p> FileResolver<'r, 'p> {
  /// Resolve `ExprKind::Ident` to a semantic identity.
  pub fn resolve_expr_ident(&self, body: &Body, expr: ExprId) -> Option<BindingId> {
    let expr = body.exprs.get(expr.0 as usize)?;
    match expr.kind {
      ExprKind::Ident(_) => self.resolver.resolve_span(self.file, expr.span),
      _ => None,
    }
  }

  /// Resolve `PatKind::Ident` (binding/assignment target) to a semantic identity.
  pub fn resolve_pat_ident(&self, body: &Body, pat: PatId) -> Option<BindingId> {
    let pat = body.pats.get(pat.0 as usize)?;
    match pat.kind {
      PatKind::Ident(_) => self.resolver.resolve_span(self.file, pat.span),
      PatKind::Assign { target, .. } => self.resolve_pat_ident(body, target),
      PatKind::AssignTarget(expr) => {
        let expr = body.exprs.get(expr.0 as usize)?;
        match expr.kind {
          ExprKind::Ident(_) => self.resolver.resolve_span(self.file, expr.span),
          _ => None,
        }
      }
      _ => None,
    }
  }
}

impl FileMap {
  fn binding_at(&self, offset: u32) -> Option<BindingId> {
    let pivot = self
      .occurrences
      .partition_point(|occ| occ.range.start <= offset);

    let mut best_containing: Option<(u32, u32, u32, u64, BindingId)> = None;
    let mut best_empty: Option<(u32, u32, u32, u64, BindingId)> = None;

    for occ in self.occurrences[..pivot].iter().rev() {
      let range = occ.range;
      let key = (
        range.len(),
        range.start,
        range.end,
        match occ.binding {
          BindingId::Def(def) => def.0,
          BindingId::Symbol(symbol) => symbol.0,
        },
        occ.binding,
      );
      if range.contains(offset) {
        if best_containing
          .map(|existing| key < existing)
          .unwrap_or(true)
        {
          best_containing = Some(key);
        }
      } else if range.is_empty() && range.start == offset {
        if best_empty.map(|existing| key < existing).unwrap_or(true) {
          best_empty = Some(key);
        }
      }
    }

    best_containing
      .or(best_empty)
      .map(|(_, _, _, _, binding)| binding)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  use typecheck_ts::{FileKey, MemoryHost};

  fn es5_host() -> MemoryHost {
    MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
      ..Default::default()
    })
  }

  #[test]
  fn resolver_builds_file_map_once() {
    let key = FileKey::new("main.ts");
    let src = r#"
export function run() {
  let x = 1;
  let y = x + x + x + x + x;
  return y;
}
"#;

    let mut host = es5_host();
    host.insert(key.clone(), src);
    let program = Program::new(host, vec![key.clone()]);
    program.check();

    let file = program.file_id(&key).unwrap();
    let lowered = program.hir_lowered(file).unwrap();
    let run_def = program
      .exports_of(file)
      .get("run")
      .and_then(|entry| entry.def)
      .expect("run is exported");
    let run_def_idx = lowered.def_index.get(&run_def).copied().unwrap();
    let run_body_id = lowered.defs[run_def_idx].body.unwrap();
    let run_body_idx = lowered.body_index.get(&run_body_id).copied().unwrap();
    let run_body = lowered.bodies[run_body_idx].as_ref();

    let resolver = Resolver::new(&program);
    let file_resolver = resolver.for_file(file);
    resolver.file_map_builds.store(0, Ordering::Relaxed);

    // Walk all expressions and resolve identifiers.
    for (id, expr) in run_body.exprs.iter().enumerate() {
      if matches!(expr.kind, ExprKind::Ident(_)) {
        let _ = file_resolver.resolve_expr_ident(run_body, ExprId(id as u32));
      }
    }

    // The map should have been built at most once for the file.
    assert_eq!(resolver.file_map_builds.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn resolver_matches_program_symbol_at_for_idents() {
    let key = FileKey::new("main.ts");
    let src = r#"
export function run() {
  let x = 1;
  return x;
}
"#;

    let mut host = es5_host();
    host.insert(key.clone(), src);
    let program = Program::new(host, vec![key.clone()]);
    program.check();

    let file = program.file_id(&key).unwrap();
    let lowered = program.hir_lowered(file).unwrap();
    let run_def = program
      .exports_of(file)
      .get("run")
      .and_then(|entry| entry.def)
      .expect("run is exported");
    let run_def_idx = lowered.def_index.get(&run_def).copied().unwrap();
    let run_body_id = lowered.defs[run_def_idx].body.unwrap();
    let run_body_idx = lowered.body_index.get(&run_body_id).copied().unwrap();
    let run_body = lowered.bodies[run_body_idx].as_ref();

    let resolver = Resolver::new(&program);
    let file_resolver = resolver.for_file(file);

    let mut checked_any = false;
    for (id, expr) in run_body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Ident(_)) {
        continue;
      }
      checked_any = true;
      let expr_id = ExprId(id as u32);
      let offset = expr.span.start;
      let symbol = program.symbol_at(file, offset).expect("symbol_at");
      let info = program.symbol_info(symbol).expect("symbol info");
      let expected = if let Some(def) = info.def {
        BindingId::Def(def)
      } else {
        BindingId::Symbol(symbol)
      };
      assert_eq!(
        file_resolver.resolve_expr_ident(run_body, expr_id),
        Some(expected)
      );
    }

    assert!(checked_any, "expected at least one identifier expression");
  }
}
