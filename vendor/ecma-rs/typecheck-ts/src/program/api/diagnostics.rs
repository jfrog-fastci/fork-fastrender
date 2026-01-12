use super::*;

impl Program {
  /// Parse, bind, and type-check all known files, returning accumulated diagnostics.
  pub fn check(&self) -> Vec<Diagnostic> {
    match self.check_fallible() {
      Ok(diags) => diags,
      Err(fatal) => self.fatal_to_diagnostics(fatal),
    }
  }

  /// Fallible entry point that surfaces unrecoverable failures to the host.
  pub fn check_fallible(&self) -> Result<Vec<Diagnostic>, FatalError> {
    self
      .collect_program_diagnostics()
      .map(|diagnostics| diagnostics.to_vec())
  }

  fn collect_program_diagnostics(&self) -> Result<Arc<[Diagnostic]>, FatalError> {
    self.catch_fatal(|| {
      self.ensure_not_cancelled()?;
      let work = {
        let mut state = self.lock_state();
        state.prepare_program_diagnostics(&self.host, &self.roots)?
      };

      match work {
        super::super::diagnostics::ProgramDiagnosticsWork::Cached(diagnostics) => Ok(diagnostics),
        super::super::diagnostics::ProgramDiagnosticsWork::Check(plan) => {
          let (cache_stats, results) = super::super::diagnostics::check_bodies_for_program(
            plan.shared_context,
            plan.body_ids,
            plan.cached_seed_results,
          );
          let mut results = results;
          // Preserve determinism regardless of parallel scheduling.
          results.sort_by_key(|(id, _)| id.0);

          // Update the in-memory body result cache under a brief write lock so
          // read-only queries (e.g. `symbol_at`) can proceed while results are
          // being committed into the salsa DB.
          let snapshot_loaded = {
            let mut state = self.lock_state();
            for (body, res) in results.iter() {
              state.body_results.insert(*body, Arc::clone(res));
            }
            if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
              state.cache_stats.lock().merge(&cache_stats);
            }
            state.snapshot_loaded
          };

          // Commit the body results into the salsa DB without holding the
          // `ProgramState` write lock. This keeps read-only queries responsive
          // while `Program::check()` is finalizing results.
          if !snapshot_loaded {
            let revision = {
              let state = self.read_state();
              for (idx, (body, res)) in results.into_iter().enumerate() {
                let db = state.typecheck_db.lock();
                let mut db = db;
                db.set_body_result(body, res);
                parking_lot::MutexGuard::unlock_fair(db);
                if idx % 64 == 0 {
                  std::thread::yield_now();
                }
              }
              let db = state.typecheck_db.lock();
              db::db_revision(&*db)
            };
            let mut state = self.lock_state();
            state.analysis_revision = Some(revision);
          }

          let (db, mut diagnostics) = {
            let state = self.read_state();
            let db = state.typecheck_db.lock().clone();
            let diagnostics = state.diagnostics.clone();
            (db, diagnostics)
          };

          let mut merged: Vec<_> = db::program_diagnostics(&db).as_ref().to_vec();
          merged.append(&mut diagnostics);
          let mut seen = std::collections::HashSet::new();
          merged.retain(|diag| {
            seen.insert((
              diag.code.clone(),
              diag.severity,
              diag.message.clone(),
              diag.primary,
            ))
          });
          super::super::diagnostics::suppress_lower0003_covered_by_ts1194(&mut merged);
          crate::codes::normalize_diagnostics(&mut merged);
          {
            let state = self.read_state();
            state.filter_skip_lib_check_diagnostics(&mut merged);
          }

          Ok(Arc::from(merged))
        }
      }
    })
  }

  /// Return collected query and cache statistics for this program.
  pub fn query_stats(&self) -> QueryStats {
    let (cache_stats, mut snapshot) = {
      let state = self.read_state();
      let mut caches = state.cache_stats.lock().clone();
      caches.merge(&state.checker_caches.stats());
      (caches, self.query_stats.snapshot())
    };

    let mut insert_cache = |kind: CacheKind, raw: &types_ts_interned::CacheStats| {
      let lookups = raw.hits + raw.misses;
      let stat = CacheStat {
        hits: raw.hits,
        misses: raw.misses,
        insertions: raw.insertions,
        evictions: raw.evictions,
        hit_rate: if lookups == 0 {
          0.0
        } else {
          raw.hits as f64 / lookups as f64
        },
      };
      snapshot.caches.insert(kind, stat);
    };

    insert_cache(CacheKind::Relation, &cache_stats.relation);
    insert_cache(CacheKind::Eval, &cache_stats.eval);
    insert_cache(CacheKind::RefExpansion, &cache_stats.ref_expansion);
    insert_cache(CacheKind::Instantiation, &cache_stats.instantiation);

    snapshot
  }
}
