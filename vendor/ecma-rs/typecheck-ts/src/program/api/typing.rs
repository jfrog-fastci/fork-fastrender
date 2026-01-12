use super::*;

impl Program {
  #[doc(hidden)]
  pub fn typecheck_db(&self) -> db::TypecheckDb {
    let state = self.read_state();
    let db = state.typecheck_db.lock().clone();
    db
  }

  /// Type for a definition.
  pub fn type_of_def(&self, def: DefId) -> TypeId {
    match self.type_of_def_fallible(def) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        self.builtin_unknown()
      }
    }
  }

  pub fn type_of_def_fallible(&self, def: DefId) -> Result<TypeId, FatalError> {
    self.catch_fatal(|| {
      self.ensure_not_cancelled()?;

      // Ensure interned tables are available before answering from caches.
      self.with_interned_state(|_| Ok(()))?;

      // Fast path: pure read access when the definition already has a stable
      // interned type.
      let cached = (|| -> Option<TypeId> {
        let state = self.read_state();
        let store = Arc::clone(&state.store);
        let prim = store.primitive_ids();

        let existing = state.interned_def_types.get(&def).copied()?;

        let ty = if store.contains_type_id(existing) {
          store.canon(existing)
        } else {
          prim.unknown
        };

        let def_data = state.def_data.get(&def);
        let is_param_def = def_data
          .and_then(|def_data| {
            state
              .hir_lowered
              .get(&def_data.file)
              .and_then(|lowered| lowered.def(def))
          })
          .map(|hir_def| hir_def.path.kind == hir_js::DefKind::Param)
          .unwrap_or(false);

        let is_self_ref = matches!(
          store.type_kind(ty),
          tti::TypeKind::Ref { def: ref_def, args }
            if args.is_empty() && ref_def.0 == def.0
        );

        // Mirror `ProgramState::type_of_def`'s cache invalidation rules: unknown
        // placeholder types and param self-references should be recomputed.
        if matches!(store.type_kind(ty), tti::TypeKind::Unknown) || (is_param_def && is_self_ref) {
          return None;
        }

        // If this is an unannotated function with an unknown cached return
        // type, allow the full `type_of_def` implementation to infer and cache
        // the return type.
        if let Some(def_data) = def_data {
          if let DefKind::Function(func) = &def_data.kind {
            if func.return_ann.is_none()
              && func.body.is_some()
              && super::super::callable_return_is_unknown(&store, ty)
            {
              let has_overloads = state.def_data.iter().any(|(other, data)| {
                *other != def
                  && data.symbol == def_data.symbol
                  && matches!(data.kind, DefKind::Function(_))
              });
              if !has_overloads {
                return None;
              }
            }
          }
        }

        Some(ty)
      })();

      if let Some(ty) = cached {
        return Ok(ty);
      }

      // Slow path: compute (and cache) the type under an exclusive lock.
      let mut state = self.lock_state();
      state.ensure_interned_types(&self.host, &self.roots)?;
      ProgramState::type_of_def(&mut state, def)
    })
  }

  /// Check a body, returning the cached result.
  pub fn check_body(&self, body: BodyId) -> Arc<BodyCheckResult> {
    match self.check_body_fallible(body) {
      Ok(res) => res,
      Err(fatal) => {
        let diagnostics = self.fatal_to_diagnostics(fatal);
        Arc::new(BodyCheckResult {
          body,
          expr_types: Vec::new(),
          call_signatures: Vec::new(),
          expr_spans: Vec::new(),
          pat_types: Vec::new(),
          pat_spans: Vec::new(),
          diagnostics,
          return_types: Vec::new(),
        })
      }
    }
  }

  pub fn check_body_fallible(&self, body: BodyId) -> Result<Arc<BodyCheckResult>, FatalError> {
    self.catch_fatal(|| {
      self.ensure_not_cancelled()?;
      let parallel_guard = db::queries::body_check::parallel_guard();
      if parallel_guard.is_some() {
        std::thread::yield_now();
      }
      let context = {
        let mut state = self.lock_state();
        state.ensure_interned_types(&self.host, &self.roots)?;
        if let Some(res) = state.body_results.get(&body).cloned() {
          if !state.snapshot_loaded {
            state.typecheck_db.lock().set_body_result(body, Arc::clone(&res));
          }
          return Ok(res);
        }
        state.body_check_context()
      };
      // Top-level bodies are used as entry points for file-wide queries like
      // `type_at_cached`. Those queries locate the innermost body at an offset,
      // which can be an `Initializer` body nested inside the top-level span.
      //
      // Ensure we seed initializer body results alongside the top-level body so
      // offset-based queries can observe types without triggering additional body
      // checks from within salsa.
      let initializer_bodies: Vec<BodyId> = match context.body_info.get(&body) {
        Some(info) if info.kind == hir_js::BodyKind::TopLevel => {
          let file = info.file;
          let mut bodies: Vec<BodyId> = context
            .body_info
            .iter()
            .filter_map(|(candidate_id, candidate)| {
              if candidate.file != file {
                return None;
              }
              if candidate.kind != hir_js::BodyKind::Initializer {
                return None;
              }
              // Only seed initializer bodies that are immediate children of this
              // top-level body. Initializers nested inside function/class bodies
              // are checked when their parent bodies are checked.
              (context.body_parents.get(candidate_id).copied() == Some(body)).then_some(*candidate_id)
            })
            .collect();
          bodies.sort_by_key(|b| b.0);
          bodies.dedup();
          bodies
        }
        _ => Vec::new(),
      };
      let db = BodyCheckDb::from_shared_context(context);
      let computed = db::queries::body_check::check_body(&db, body);
      for init_body in initializer_bodies.iter().copied() {
        let _ = db::queries::body_check::check_body(&db, init_body);
      }
      let mut state = self.lock_state();
      let res = state
        .body_results
        .entry(body)
        .or_insert_with(|| Arc::clone(&computed))
        .clone();
      state.cache_body_result(body, Arc::clone(&res));
      for init_body in initializer_bodies {
        if init_body == body {
          continue;
        }
        let init_res = db::queries::body_check::check_body(&db, init_body);
        state.cache_body_result(init_body, init_res);
      }
      Ok(res)
    })
  }

  /// Type of a specific expression in a body.
  pub fn type_of_expr(&self, body: BodyId, expr: ExprId) -> TypeId {
    match self.type_of_expr_fallible(body, expr) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        self.builtin_unknown()
      }
    }
  }

  pub fn type_of_expr_fallible(&self, body: BodyId, expr: ExprId) -> Result<TypeId, FatalError> {
    let result = self.check_body_fallible(body)?;
    let unknown = self.with_interned_state(|state| Ok(state.interned_unknown()))?;
    Ok(result.expr_type(expr).unwrap_or(unknown))
  }
}
