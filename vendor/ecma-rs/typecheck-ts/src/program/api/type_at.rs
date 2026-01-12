use super::*;

impl Program {
  /// Innermost expression covering an offset within a file.
  pub fn expr_at(&self, file: FileId, offset: u32) -> Option<(BodyId, ExprId)> {
    match self.expr_at_fallible(file, offset) {
      Ok(expr) => expr,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn expr_at_fallible(
    &self,
    file: FileId,
    offset: u32,
  ) -> Result<Option<(BodyId, ExprId)>, FatalError> {
    self.with_analyzed_state(|state| Ok(state.expr_at(file, offset)))
  }

  /// Type of the innermost expression covering an offset within a file.
  pub fn type_at(&self, file: FileId, offset: u32) -> Option<TypeId> {
    match self.type_at_fallible(file, offset) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn type_at_fallible(&self, file: FileId, offset: u32) -> Result<Option<TypeId>, FatalError> {
    self.catch_fatal(|| {
      self.ensure_not_cancelled()?;
      // Ensure interned declarations and maps are ready before we attempt span
      // lookups or body checking below.
      self.with_interned_state(|_| Ok(()))?;

      const TYPE_AT_TRIVIA_LOOKAROUND: usize = 32;

      let (store, offset, expr_at, pat_at) = {
        let state = self.read_state();
        let store = Arc::clone(&state.store);
        let mut offset = offset;
        let mut expr_at = state.expr_at(file, offset);
        let mut pat_at = state.pat_at(file, offset);

        if expr_at.is_none() && pat_at.is_none() {
          if let Ok(text) = state.load_text(file, &self.host) {
            let bytes = text.as_bytes();
            let start = (offset as usize).min(bytes.len());

            let mut found = None;
            for step in 1..=TYPE_AT_TRIVIA_LOOKAROUND {
              if start < step {
                break;
              }
              let cand = start - step;
              let Ok(cand_u32) = cand.try_into() else {
                break;
              };
              if state.expr_at(file, cand_u32).is_some() || state.pat_at(file, cand_u32).is_some() {
                found = Some(cand_u32);
                break;
              }
            }

            if found.is_none() {
              for step in 1..=TYPE_AT_TRIVIA_LOOKAROUND {
                let cand = start + step;
                if cand >= bytes.len() {
                  break;
                }
                let Ok(cand_u32) = cand.try_into() else {
                  break;
                };
                if state.expr_at(file, cand_u32).is_some() || state.pat_at(file, cand_u32).is_some()
                {
                  found = Some(cand_u32);
                  break;
                }
              }
            }

            if let Some(adj) = found {
              offset = adj;
              expr_at = state.expr_at(file, offset);
              pat_at = state.pat_at(file, offset);
            }
          }
        }

        (store, offset, expr_at, pat_at)
      };

      let unknown = store.primitive_ids().unknown;

      let (body, expr) = match expr_at {
        Some(res) => res,
        None => {
          let Some((body, pat)) = pat_at else {
            return Ok(None);
          };
          let result = self.check_body_fallible(body)?;
          let ty = result.pat_type(pat).unwrap_or(unknown);
          let ty = if store.contains_type_id(ty) {
            store.canon(ty)
          } else {
            unknown
          };
          return Ok(Some(ty));
        }
      };

      let result = self.check_body_fallible(body)?;
      let (expr, mut ty) = match result.expr_at(offset) {
        Some((expr_id, ty)) => (expr_id, ty),
        None => (expr, result.expr_type(expr).unwrap_or(unknown)),
      };

      let mut member_fallback: Option<(bool, TypeId, String)> = None;
      let mut binding_def: Option<DefId> = None;
      let mut binding_ty: Option<TypeId> = None;
      let mut contextual_ty: Option<TypeId> = None;
      let mut expr_def_fallback: Option<DefId> = None;

      {
        let state = self.read_state();
        if let Some(meta) = state.body_map.get(&body).copied() {
          if let Some(hir_id) = meta.hir {
            if let Some(lowered) = state.hir_lowered.get(&meta.file) {
              if let Some(hir_body) = lowered.body(hir_id) {
                if let Some(expr_data) = hir_body.exprs.get(expr.0 as usize) {
                  match &expr_data.kind {
                    HirExprKind::Ident(name_id) => {
                      if let Some(name) = lowered.names.resolve(*name_id) {
                        if let Some(file_state) = state.files.get(&meta.file) {
                          if let Some(binding) = file_state.bindings.get(name) {
                            binding_def = binding.def;
                            binding_ty = binding.type_id;
                          }
                        }
                      }
                    }
                    HirExprKind::Member(mem) => {
                      let key = match &mem.property {
                        hir_js::ObjectKey::Ident(id) => {
                          lowered.names.resolve(*id).map(|s| s.to_string())
                        }
                        hir_js::ObjectKey::String(s) => Some(s.clone()),
                        hir_js::ObjectKey::Number(n) => Some(n.clone()),
                        hir_js::ObjectKey::Computed(_) => None,
                      };
                      if let Some(key) = key {
                        let base_ty = result.expr_type(mem.object).unwrap_or(unknown);
                        member_fallback = Some((mem.optional, base_ty, key));
                      }
                    }
                    HirExprKind::FunctionExpr { def, .. } => {
                      expr_def_fallback = Some(*def);
                    }
                    HirExprKind::ClassExpr { def, .. } => {
                      expr_def_fallback = state.value_defs.get(def).copied();
                    }
                    _ => {}
                  }
                  if contextual_ty.is_none() {
                    for candidate in hir_body.exprs.iter() {
                      if let HirExprKind::Call(call) = &candidate.kind {
                        if let Some(arg_idx) = call.args.iter().position(|arg| arg.expr.0 == expr.0)
                        {
                          if let Some(callee_ty) = result.expr_type(call.callee) {
                            let sigs = callable_signatures(store.as_ref(), callee_ty);
                            if let Some(sig_id) = sigs.first() {
                              let sig = store.signature(*sig_id);
                              if let Some(param) = sig.params.get(arg_idx) {
                                contextual_ty = Some(param.ty);
                                break;
                              }
                            }
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }

      if let Some(ctx) = contextual_ty {
        ty = ctx;
      }

      let is_unknown =
        !store.contains_type_id(ty) || matches!(store.type_kind(store.canon(ty)), tti::TypeKind::Unknown);
      let should_resolve_binding = is_unknown
        || (store.contains_type_id(ty)
          && matches!(
            store.type_kind(store.canon(ty)),
            tti::TypeKind::Ref { def, args }
              if args.is_empty() && binding_def.map(|bd| bd.0 == def.0).unwrap_or(false)
          ));
      if should_resolve_binding {
        if let Some(def) = binding_def {
          match self.type_of_def_fallible(def) {
            Ok(def_ty) => {
              ty = if store.contains_type_id(def_ty) {
                store.canon(def_ty)
              } else {
                unknown
              };
            }
            Err(FatalError::Cancelled) => return Err(FatalError::Cancelled),
            Err(_) => {}
          }
        }
        let still_unknown = !store.contains_type_id(ty)
          || matches!(store.type_kind(store.canon(ty)), tti::TypeKind::Unknown);
        if still_unknown {
          if let Some(binding_ty) = binding_ty {
            ty = binding_ty;
          }
        }
      }

      let member_fallback_allowed = !store.contains_type_id(ty)
        || matches!(store.type_kind(store.canon(ty)), tti::TypeKind::Unknown);
      if member_fallback_allowed {
        if let Some((optional, base_ty, key)) = member_fallback {
          let prop_ty = {
            let state = self.read_state();
            let caches = state.checker_caches.for_body();
            let expander = RefExpander::new(
              Arc::clone(&store),
              &state.interned_def_types,
              &state.interned_type_params,
              &state.interned_type_param_decls,
              &state.interned_intrinsics,
              &state.interned_class_instances,
              caches.eval.clone(),
            );
            let mut prop_ty =
              lookup_interned_property_type(store.as_ref(), Some(&expander), base_ty, &key);
            if prop_ty.is_none() {
              if let tti::TypeKind::Ref { def, .. } = store.type_kind(store.canon(base_ty)) {
                if let Some(mapped) = state.interned_def_types.get(&DefId(def.0)).copied() {
                  prop_ty = lookup_interned_property_type(store.as_ref(), None, mapped, &key);
                }
              }
            }
            prop_ty.map(|prop_ty| {
              if optional {
                store.union(vec![prop_ty, store.primitive_ids().undefined])
              } else {
                prop_ty
              }
            })
          };
          if let Some(prop_ty) = prop_ty {
            ty = prop_ty;
          }
        }
      }

      // `BodyCheckResult` currently types function/class expressions as `unknown` (or functions as
      // `(...args) => unknown`) when they are uncontextualized. Use the expression's stable `DefId`
      // to recover the proper declaration type on demand.
      if let Some(def) = expr_def_fallback {
        let ty_is_unknown =
          !store.contains_type_id(ty) || matches!(store.type_kind(store.canon(ty)), tti::TypeKind::Unknown);
        let ty_is_underspecified =
          store.contains_type_id(ty) && super::super::callable_return_is_unknown(&store, ty);
        if ty_is_unknown || ty_is_underspecified {
          match self.type_of_def_fallible(def) {
            Ok(def_ty) => {
              ty = if store.contains_type_id(def_ty) {
                store.canon(def_ty)
              } else {
                unknown
              };
            }
            Err(FatalError::Cancelled) => return Err(FatalError::Cancelled),
            Err(_) => {}
          }
        }
      }

      if std::env::var("DEBUG_TYPE_AT").is_ok() {
        if let Some(span) = result.expr_span(expr) {
          eprintln!(
            "type_at debug: body {:?} expr {:?} span {:?}",
            body, expr, span
          );
        } else {
          eprintln!("type_at debug: body {:?} expr {:?} (no span)", body, expr);
        }

        let (meta_kind, hir_expr_kind, parent_body, owner_name, parent_owner_name) = {
          let state = self.read_state();
          let meta_kind = state.body_map.get(&body).map(|meta| meta.kind);
          let hir_expr_kind = state
            .body_map
            .get(&body)
            .and_then(|meta| meta.hir)
            .and_then(|hir_id| state.hir_lowered.get(&file).and_then(|lowered| lowered.body(hir_id)))
            .and_then(|hir_body| hir_body.exprs.get(expr.0 as usize))
            .map(|expr_data| format!("{:?}", expr_data.kind));
          let parent_body = state.body_parents.get(&body).copied();
          let owner_name = state
            .owner_of_body(body)
            .and_then(|owner| state.def_data.get(&owner))
            .map(|def| def.name.clone());
          let parent_owner_name = parent_body
            .and_then(|parent| state.owner_of_body(parent))
            .and_then(|owner| state.def_data.get(&owner))
            .map(|def| def.name.clone());
          (meta_kind, hir_expr_kind, parent_body, owner_name, parent_owner_name)
        };

        if let Some(kind) = meta_kind {
          eprintln!("  meta kind {:?}", kind);
        }
        if let Some(kind) = hir_expr_kind {
          eprintln!("  hir expr kind {}", kind);
        }
        eprintln!("  parent {:?}", parent_body);
        if let Some(raw_ty) = result.expr_type(expr) {
          if store.contains_type_id(raw_ty) {
            eprintln!("  raw type {:?}", store.type_kind(raw_ty));
          } else {
            eprintln!("  raw type {:?}", raw_ty);
          }
        }

        if let Some(parent) = parent_body {
          match self.check_body_fallible(parent) {
            Ok(parent_res) => {
              eprintln!("  parent pat types {:?}", parent_res.pat_types());
              if let Some(first) = parent_res.pat_types().first() {
                if store.contains_type_id(*first) {
                  eprintln!("  parent pat kind {:?}", store.type_kind(*first));
                }
              }
            }
            Err(FatalError::Cancelled) => return Err(FatalError::Cancelled),
            Err(_) => {}
          }
        }

        if let Some(owner_name) = owner_name {
          eprintln!("  owner {:?}", owner_name);
        }
        if let Some(parent_owner_name) = parent_owner_name {
          eprintln!("  parent owner {:?}", parent_owner_name);
        }
      }

      let is_number_literal = store.contains_type_id(ty)
        && matches!(
          store.type_kind(store.canon(ty)),
          tti::TypeKind::NumberLiteral(_)
        );
      if is_number_literal {
        let is_literal = {
          let state = self.read_state();
          state
            .body_map
            .get(&body)
            .and_then(|meta| meta.hir)
            .and_then(|hir_id| {
              state
                .hir_lowered
                .get(&file)
                .and_then(|lowered| lowered.body(hir_id))
                .and_then(|hir_body| {
                  hir_body
                    .exprs
                    .get(expr.0 as usize)
                    .map(|expr_data| matches!(expr_data.kind, HirExprKind::Literal(_)))
                })
            })
            .unwrap_or(false)
        };
        if is_literal {
          let best = {
            let state = self.read_state();
            state.body_map.get(&body).and_then(|meta| {
              meta.hir.and_then(|hir_id| {
                state.hir_lowered.get(&meta.file).and_then(|lowered| {
                  lowered.body(hir_id).and_then(|hir_body| {
                    let mut best: Option<(u32, TypeId)> = None;
                    for (idx, expr_data) in hir_body.exprs.iter().enumerate() {
                      let span = expr_data.span;
                      if !(span.start <= offset && offset < span.end) {
                        continue;
                      }
                      if let HirExprKind::Binary { op, .. } = &expr_data.kind {
                        let numeric = matches!(
                          op,
                          HirBinaryOp::Add
                            | HirBinaryOp::Subtract
                            | HirBinaryOp::Multiply
                            | HirBinaryOp::Divide
                            | HirBinaryOp::Exponent
                            | HirBinaryOp::Remainder
                            | HirBinaryOp::BitAnd
                            | HirBinaryOp::BitOr
                            | HirBinaryOp::BitXor
                            | HirBinaryOp::ShiftLeft
                            | HirBinaryOp::ShiftRight
                            | HirBinaryOp::ShiftRightUnsigned
                        );
                        if !numeric {
                          continue;
                        }
                        let len = span.len();
                        let bin_ty = result.expr_type(ExprId(idx as u32)).unwrap_or(ty);
                        let is_number = store.contains_type_id(bin_ty)
                          && matches!(
                            store.type_kind(store.canon(bin_ty)),
                            tti::TypeKind::Number
                          );
                        if is_number {
                          let replace = best.map(|(l, _)| len < l).unwrap_or(true);
                          if replace {
                            best = Some((len, bin_ty));
                          }
                        }
                      }
                    }
                    best
                  })
                })
              })
            })
          };
          if let Some((_, bin_ty)) = best {
            ty = bin_ty;
          }
        }
      }

      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        unknown
      };
      Ok(Some(ty))
    })
  }

  /// Type of the innermost expression at the given offset, using cached body results.
  ///
  /// Unlike [`Program::type_at`], this will **not** trigger body checking; it only
  /// consults results previously seeded into the program's internal salsa
  /// database by [`Program::check_body`](crate::Program::check_body).
  ///
  /// Returns `None` when no cached [`BodyCheckResult`] is available (for example,
  /// when the relevant body has not been checked yet).
  pub fn type_at_cached(&self, file: FileId, offset: u32) -> Option<TypeId> {
    match self.type_at_cached_fallible(file, offset) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn type_at_cached_fallible(
    &self,
    file: FileId,
    offset: u32,
  ) -> Result<Option<TypeId>, FatalError> {
    self.with_interned_state(|state| {
      if state.snapshot_loaded {
        let Some((body, expr)) = state.expr_at(file, offset) else {
          return Ok(None);
        };
        let Some(result) = state.body_results.get(&body) else {
          return Ok(None);
        };
        if let Some((_, ty)) = result.expr_at(offset) {
          return Ok(Some(ty));
        }
        return Ok(result.expr_type(expr));
      }

      let db = state.typecheck_db.lock().clone();
      Ok(db::type_at(&db, file, offset))
    })
  }

  /// Resolved signature for the innermost call/construct expression covering an offset.
  pub fn call_signature_at(&self, file: FileId, offset: u32) -> Option<tti::SignatureId> {
    match self.call_signature_at_fallible(file, offset) {
      Ok(sig) => sig,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn call_signature_at_fallible(
    &self,
    file: FileId,
    offset: u32,
  ) -> Result<Option<tti::SignatureId>, FatalError> {
    self.catch_fatal(|| {
      self.ensure_not_cancelled()?;
      self.with_interned_state(|_| Ok(()))?;

      const CALL_SIGNATURE_AT_TRIVIA_LOOKAROUND: usize = 32;

      let (offset, expr_at) = {
        let state = self.read_state();
        let mut offset = offset;
        let mut expr_at = state.expr_at(file, offset);

        if expr_at.is_none() {
          if let Ok(text) = state.load_text(file, &self.host) {
            let bytes = text.as_bytes();
            let start = (offset as usize).min(bytes.len());

            let mut found = None;
            for step in 1..=CALL_SIGNATURE_AT_TRIVIA_LOOKAROUND {
              if start < step {
                break;
              }
              let cand = start - step;
              let Ok(cand_u32) = cand.try_into() else {
                break;
              };
              if state.expr_at(file, cand_u32).is_some() {
                found = Some(cand_u32);
                break;
              }
            }

            if found.is_none() {
              for step in 1..=CALL_SIGNATURE_AT_TRIVIA_LOOKAROUND {
                let cand = start + step;
                if cand >= bytes.len() {
                  break;
                }
                let Ok(cand_u32) = cand.try_into() else {
                  break;
                };
                if state.expr_at(file, cand_u32).is_some() {
                  found = Some(cand_u32);
                  break;
                }
              }
            }

            if let Some(adj) = found {
              offset = adj;
              expr_at = state.expr_at(file, offset);
            }
          }
        }

        (offset, expr_at)
      };

      let Some((body, expr)) = expr_at else {
        return Ok(None);
      };
      let result = self.check_body_fallible(body)?;
      let expr = result
        .expr_at(offset)
        .map(|(expr_id, _)| expr_id)
        .unwrap_or(expr);
      Ok(result.call_signature(expr))
    })
  }
}
