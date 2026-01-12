use super::*;

pub(super) enum ProgramDiagnosticsWork {
  Cached(Arc<[Diagnostic]>),
  Check(ProgramDiagnosticsPlan),
}

pub(super) struct ProgramDiagnosticsPlan {
  pub(super) body_ids: Vec<BodyId>,
  pub(super) shared_context: Arc<BodyCheckContext>,
  pub(super) cached_seed_results: Vec<(BodyId, Arc<BodyCheckResult>)>,
}

pub(super) fn check_bodies_for_program(
  shared_context: Arc<BodyCheckContext>,
  body_ids: Vec<BodyId>,
  cached_seed_results: Vec<(BodyId, Arc<BodyCheckResult>)>,
) -> (CheckerCacheStats, Vec<(BodyId, Arc<BodyCheckResult>)>) {
  // Parent body results (especially top-level bodies) are needed to seed bindings for many
  // child bodies. Compute these sequentially once and seed each parallel worker with the
  // results to avoid redundant work (and pathological contention) during parallel checking.
  let mut seed_results = cached_seed_results;
  let mut seeded_ids: HashSet<BodyId> = seed_results.iter().map(|(id, _)| *id).collect();
  let mut remaining: Vec<BodyId> = Vec::with_capacity(body_ids.len());
  let seed_db = BodyCheckDb::from_shared_context_with_seed_results(
    Arc::clone(&shared_context),
    seed_results.as_slice(),
  );
  for body in body_ids.iter().copied() {
    let is_top_level = shared_context
      .body_info
      .get(&body)
      .is_some_and(|info| matches!(info.kind, HirBodyKind::TopLevel));
    if is_top_level {
      if seeded_ids.insert(body) {
        seed_results.push((body, db::queries::body_check::check_body(&seed_db, body)));
      }
    } else {
      remaining.push(body);
    }
  }
  // Preserve determinism regardless of which top-level results were already cached.
  seed_results.sort_by_key(|(id, _)| id.0);
  let seed_cache_stats = seed_db.into_cache_stats();
  let seed_results = Arc::new(seed_results);

  // `program_diagnostics` is used heavily in fuzz/proptests where the program often contains
  // only a handful of bodies. Spawning parallel body-check workers in those scenarios can be
  // slower than checking sequentially because each worker needs its own `BodyCheckDb` (and thus
  // its own per-thread memoization tables).
  //
  // Prefer a fast sequential path when the workload is small; keep the parallel path for larger
  // programs where the extra setup amortizes.
  const PARALLEL_BODY_CHECK_THRESHOLD: usize = 64;
  let (cache_stats, mut results): (CheckerCacheStats, Vec<(BodyId, Arc<BodyCheckResult>)>) = if
    remaining.len() <= PARALLEL_BODY_CHECK_THRESHOLD
  {
    let db = BodyCheckDb::from_shared_context_with_seed_results(
      Arc::clone(&shared_context),
      seed_results.as_slice(),
    );
    let mut results = Vec::with_capacity(remaining.len());
    for body in remaining.iter().copied() {
      results.push((body, db::queries::body_check::check_body(&db, body)));
    }
    (db.into_cache_stats(), results)
  } else {
    use rayon::prelude::*;
    remaining
      .par_iter()
      .fold(
        || {
          (
            BodyCheckDb::from_shared_context_with_seed_results(
              Arc::clone(&shared_context),
              seed_results.as_slice(),
            ),
            Vec::new(),
          )
        },
        |(db, mut results), body| {
          results.push((*body, db::queries::body_check::check_body(&db, *body)));
          (db, results)
        },
      )
      .map(|(db, results)| (db.into_cache_stats(), results))
      .reduce(
        || (CheckerCacheStats::default(), Vec::new()),
        |(mut stats, mut merged), (thread_stats, results)| {
          stats.merge(&thread_stats);
          merged.extend(results);
          (stats, merged)
        },
      )
  };

  results.extend(seed_results.iter().map(|(id, res)| (*id, Arc::clone(res))));
  let mut cache_stats = cache_stats;
  cache_stats.merge(&seed_cache_stats);

  // Preserve determinism regardless of parallel scheduling.
  results.sort_by_key(|(id, _)| id.0);
  (cache_stats, results)
}

/// `hir-js` emits `LOWER0003` warnings for export statements that are not at
/// module top level. TypeScript, however, permits parsing `export { ... }` /
/// `export * ...` inside internal namespaces/modules and reports TS1194
/// instead.
///
/// Once TS1194 is present, suppress redundant lowering warnings so callers see
/// the tsc-aligned diagnostic set.
pub(super) fn suppress_lower0003_covered_by_ts1194(diagnostics: &mut Vec<Diagnostic>) {
  let ts1194_spans: Vec<Span> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == "TS1194")
    .map(|diag| diag.primary)
    .collect();
  if ts1194_spans.is_empty() {
    return;
  }

  diagnostics.retain(|diag| {
    if diag.code.as_str() != "LOWER0003" {
      return true;
    }
    !ts1194_spans.iter().any(|span| {
      span.file == diag.primary.file
        && ((span.range.start >= diag.primary.range.start && span.range.end <= diag.primary.range.end)
          || (diag.primary.range.start >= span.range.start && diag.primary.range.end <= span.range.end))
    })
  });
}

pub(super) fn merge_program_diagnostics(
  db: &db::TypecheckDb,
  mut extra_diagnostics: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
  let mut merged: Vec<_> = db::program_diagnostics(db).as_ref().to_vec();
  merged.append(&mut extra_diagnostics);
  let mut seen = HashSet::new();
  merged.retain(|diag| {
    seen.insert((
      diag.code.clone(),
      diag.severity,
      diag.message.clone(),
      diag.primary,
    ))
  });
  suppress_lower0003_covered_by_ts1194(&mut merged);
  codes::normalize_diagnostics(&mut merged);
  merged
}

impl ProgramState {
  pub(super) fn filter_skip_lib_check_diagnostics(&self, diagnostics: &mut Vec<Diagnostic>) {
    if !self.compiler_options.skip_lib_check {
      return;
    }

    diagnostics.retain(|diag| {
      if self.file_kinds.get(&diag.primary.file) != Some(&FileKind::Dts) {
        return true;
      }
      let code = diag.code.as_str();

      // `skipLibCheck` matches `tsc` by suppressing semantic diagnostics
      // originating from `.d.ts` files, including module resolution failures
      // and triple-slash reference errors inside declaration files.
      //
      // Keep diagnostics that are not tied to semantic checking (parse errors,
      // host errors, etc).
      //
      // Note: this intentionally supersedes an earlier plan (Task 183) to keep
      // some `.d.ts` resolution/reference errors when `skipLibCheck` is enabled.

      if code.starts_with("TC") || code.starts_with("BIND") || code.starts_with("TS") {
        return false;
      }

      true
    });
  }

  pub(super) fn prepare_program_diagnostics(
    &mut self,
    host: &Arc<dyn Host>,
    roots: &[FileKey],
  ) -> Result<ProgramDiagnosticsWork, FatalError> {
    if self.snapshot_loaded {
      let mut diagnostics = self.diagnostics.clone();
      self.filter_skip_lib_check_diagnostics(&mut diagnostics);
      return Ok(ProgramDiagnosticsWork::Cached(Arc::from(diagnostics)));
    }
    self.check_cancelled()?;
    self.ensure_analyzed_result(host, roots)?;
    let prev_decl_fingerprint = self.decl_types_fingerprint;
    self.ensure_interned_types(host, roots)?;
    self.set_extra_diagnostics_input();
    let can_reuse_cached_bodies = self.decl_types_fingerprint == prev_decl_fingerprint;

    let body_ids: Vec<_> = {
      let db = self.typecheck_db.lock().clone();
      let mut body_ids: Vec<_> = db::body_to_file(&db)
        .iter()
        .filter_map(|(body, file)| {
          let kind = db::file_kind(&db, *file);
          (!matches!(kind, FileKind::Dts)).then_some(*body)
        })
        .collect();
      body_ids.sort_by_key(|id| id.0);
      body_ids
    };
    let shared_context = self.body_check_context();
    let mut cached_seed_results: Vec<(BodyId, Arc<BodyCheckResult>)> = Vec::new();
    if can_reuse_cached_bodies {
      for body in body_ids.iter().copied() {
        let is_top_level = shared_context
          .body_info
          .get(&body)
          .is_some_and(|info| matches!(info.kind, HirBodyKind::TopLevel));
        if !is_top_level {
          continue;
        }
        if let Some(res) = self.body_results.get(&body) {
          cached_seed_results.push((body, Arc::clone(res)));
        }
      }
    }

    let body_ids: Vec<BodyId> = if can_reuse_cached_bodies {
      body_ids
        .iter()
        .copied()
        .filter(|body| !self.body_results.contains_key(body))
        .collect()
    } else {
      body_ids
    };
    Ok(ProgramDiagnosticsWork::Check(ProgramDiagnosticsPlan {
      body_ids,
      shared_context,
      cached_seed_results,
    }))
  }

  #[cfg(feature = "serde")]
  pub(super) fn commit_body_results_for_diagnostics(
    &mut self,
    cache_stats: CheckerCacheStats,
    mut results: Vec<(BodyId, Arc<BodyCheckResult>)>,
  ) -> Result<(), FatalError> {
    // Preserve determinism regardless of parallel scheduling.
    results.sort_by_key(|(id, _)| id.0);
    {
      let mut db = self.typecheck_db.lock();
      for (body, res) in results {
        self.body_results.insert(body, Arc::clone(&res));
        if !self.snapshot_loaded {
          db.set_body_result(body, res);
        }
      }
      // `ensure_analyzed_result` uses `analysis_revision` to avoid redundant work.
      // Body results are part of the salsa inputs, so they advance the revision.
      // Keep `analysis_revision` in sync so subsequent queries can reuse analysis.
      self.analysis_revision = Some(db::db_revision(&*db));
    }
    if matches!(self.compiler_options.cache.mode, CacheMode::PerBody) {
      self.cache_stats.lock().merge(&cache_stats);
    }
    Ok(())
  }
}
