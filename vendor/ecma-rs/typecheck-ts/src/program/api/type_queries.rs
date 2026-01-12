use super::*;

impl Program {
  /// Shared interned `types-ts-interned` store used by this program.
  ///
  /// Downstream tooling (e.g. `effect-js`) may need direct access to the store
  /// to inspect `TypeKind` values returned by `BodyCheckResult` queries.
  pub fn interned_type_store(&self) -> Arc<tti::TypeStore> {
    match self.with_interned_state(|state| Ok(Arc::clone(&state.store))) {
      Ok(store) => store,
      Err(fatal) => {
        self.record_fatal(fatal);
        let state = self.lock_state();
        Arc::clone(&state.store)
      }
    }
  }

  /// Interned type of a definition, using the `types-ts-interned` store.
  pub fn type_of_def_interned(&self, def: DefId) -> TypeId {
    match self.type_of_def_interned_fallible(def) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        let state = self.lock_state();
        state.store.primitive_ids().unknown
      }
    }
  }

  /// Interned, *expanded* type of a definition.
  ///
  /// For named type declarations (type aliases, interfaces, classes, enums),
  /// [`Program::type_of_def_interned`] returns a `TypeKind::Ref` pointing at the
  /// definition itself so callers can preserve the name. This helper returns
  /// the stored definition type instead (e.g. the RHS of a type alias).
  pub fn declared_type_of_def_interned(&self, def: DefId) -> TypeId {
    match self.declared_type_of_def_interned_fallible(def) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        let state = self.lock_state();
        state.store.primitive_ids().unknown
      }
    }
  }

  pub fn declared_type_of_def_interned_fallible(&self, def: DefId) -> Result<TypeId, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let expanded = match state.interned_def_types.get(&def).copied() {
        Some(existing) if !matches!(store.type_kind(existing), tti::TypeKind::Unknown) => existing,
        _ => {
          ProgramState::type_of_def(state, def)?;
          state
            .interned_def_types
            .get(&def)
            .copied()
            .unwrap_or(store.primitive_ids().unknown)
        }
      };
      Ok(store.canon(expanded))
    })
  }

  pub fn type_of_def_interned_fallible(&self, def: DefId) -> Result<TypeId, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let expanded = match state.interned_def_types.get(&def).copied() {
        Some(existing) if !matches!(store.type_kind(existing), tti::TypeKind::Unknown) => existing,
        _ => {
          ProgramState::type_of_def(state, def)?;
          state
            .interned_def_types
            .get(&def)
            .copied()
            .unwrap_or(store.primitive_ids().unknown)
        }
      };
      let wants_named_ref = state
        .def_data
        .get(&def)
        .map(|data| {
          matches!(
            data.kind,
            DefKind::Interface(_) | DefKind::TypeAlias(_) | DefKind::Class(_) | DefKind::Enum(_)
          )
        })
        .unwrap_or(false);
      if wants_named_ref {
        let mut args = state
          .interned_type_params
          .get(&def)
          .cloned()
          .unwrap_or_default();
        args.sort_by_key(|param| param.0);
        let args: Vec<_> = args
          .into_iter()
          .map(|param| store.intern_type(tti::TypeKind::TypeParam(param)))
          .collect();
        return Ok(store.canon(store.intern_type(tti::TypeKind::Ref { def, args })));
      }
      Ok(expanded)
    })
  }

  /// Expanded kind summary for an interned type.
  pub fn type_kind(&self, ty: TypeId) -> TypeKindSummary {
    match self.type_kind_fallible(ty) {
      Ok(kind) => kind,
      Err(fatal) => {
        self.record_fatal(fatal);
        TypeKindSummary::Unknown
      }
    }
  }

  pub fn type_kind_fallible(&self, ty: TypeId) -> Result<TypeKindSummary, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let result = queries.type_kind(ty);
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(result)
    })
  }

  /// Raw interned type kind without expansion.
  pub fn interned_type_kind(&self, ty: TypeId) -> tti::TypeKind {
    match self.interned_type_kind_fallible(ty) {
      Ok(kind) => kind,
      Err(fatal) => {
        self.record_fatal(fatal);
        tti::TypeKind::Unknown
      }
    }
  }

  pub fn interned_type_kind_fallible(&self, ty: TypeId) -> Result<tti::TypeKind, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      Ok(store.type_kind(ty))
    })
  }

  /// Evaluate an interned type by expanding `TypeKind::Ref` nodes and reducing
  /// type operators (conditional/mapped/template/indexed/keyof).
  ///
  /// This is primarily intended for ahead-of-time backends (e.g. native codegen)
  /// that need to compute deterministic layouts for concrete types.
  ///
  /// If `ty` is not a valid [`TypeId`] for this program's interned
  /// [`types_ts_interned::TypeStore`], `unknown` is returned.
  pub fn evaluate_type_interned(&self, ty: TypeId) -> TypeId {
    match self.evaluate_type_interned_fallible(ty) {
      Ok(ty) => ty,
      Err(fatal) => {
        self.record_fatal(fatal);
        let state = self.lock_state();
        state.store.primitive_ids().unknown
      }
    }
  }

  pub fn evaluate_type_interned_fallible(&self, ty: TypeId) -> Result<TypeId, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let evaluated = store.canon(queries.evaluate(ty));
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(evaluated)
    })
  }

  /// Deterministic list of union member types for `ty` after evaluation.
  ///
  /// The input is first evaluated via [`Program::evaluate_type_interned`]. If
  /// the evaluated type is a union, the canonicalized member `TypeId`s are
  /// returned in stable order. Otherwise this returns an empty list.
  ///
  /// If `ty` is not a valid [`TypeId`] for this program's interned
  /// [`types_ts_interned::TypeStore`], an empty list is returned.
  pub fn union_members_interned(&self, ty: TypeId) -> Vec<TypeId> {
    match self.union_members_interned_fallible(ty) {
      Ok(members) => members,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  pub fn union_members_interned_fallible(&self, ty: TypeId) -> Result<Vec<TypeId>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let evaluated = store.canon(queries.evaluate(ty));
      let members = match store.type_kind(evaluated) {
        tti::TypeKind::Union(members) => members.into_iter().map(|ty| store.canon(ty)).collect(),
        _ => Vec::new(),
      };
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(members)
    })
  }

  /// Compute the deterministic native runtime [`types_ts_interned::LayoutId`] for
  /// an interned type.
  ///
  /// Unlike [`types_ts_interned::TypeStore::layout_of`], this method first
  /// evaluates `ty` (expanding `TypeKind::Ref` and reducing type operators) so
  /// callers do not need to remember to expand references before asking for
  /// layouts.
  ///
  /// If `ty` is not a valid [`TypeId`] for this program's interned
  /// [`types_ts_interned::TypeStore`], the layout for `unknown` is returned.
  pub fn layout_of_interned(&self, ty: TypeId) -> tti::LayoutId {
    match self.layout_of_interned_fallible(ty) {
      Ok(layout) => layout,
      Err(fatal) => {
        self.record_fatal(fatal);
        let state = self.lock_state();
        let unknown = state.store.primitive_ids().unknown;
        state.store.layout_of(unknown)
      }
    }
  }

  pub fn layout_of_interned_fallible(&self, ty: TypeId) -> Result<tti::LayoutId, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let evaluated = store.canon(queries.evaluate(ty));
      let layout = store.layout_of(evaluated);
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(layout)
    })
  }

  /// Explain why `src` is not assignable to `dst`.
  ///
  /// Returns `None` if `src` is assignable to `dst`.
  pub fn explain_assignability(&self, src: TypeId, dst: TypeId) -> Option<ExplainTree> {
    match self.explain_assignability_fallible(src, dst) {
      Ok(tree) => tree,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn explain_assignability_fallible(
    &self,
    src: TypeId,
    dst: TypeId,
  ) -> Result<Option<ExplainTree>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let src = if store.contains_type_id(src) {
        store.canon(src)
      } else {
        store.primitive_ids().unknown
      };
      let dst = if store.contains_type_id(dst) {
        store.canon(dst)
      } else {
        store.primitive_ids().unknown
      };
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
      let hooks = relate_hooks();
      let hooks = tti::RelateHooks {
        expander: Some(&expander),
        is_same_origin_private_member: hooks.is_same_origin_private_member,
        check_cancelled: hooks.check_cancelled,
      };
      // Use a fresh relation cache so explanation trees contain full structure
      // instead of "cached" sentinel nodes from prior checker passes.
      let relation_cache = tti::RelationCache::new(state.compiler_options.cache.relation_config());
      let options = store.options();
      let ctx = RelateCtx::with_hooks_cache_and_normalizer_caches(
        Arc::clone(&store),
        options,
        hooks,
        relation_cache,
        caches.eval.clone(),
      );

      let result = ctx.explain_assignable(src, dst);
      if result.result {
        return Ok(None);
      }

      Ok(result.reason.or_else(|| {
        Some(tti::ReasonNode {
          src,
          dst,
          relation: tti::RelationKind::Assignable,
          outcome: false,
          note: Some("no explanation available".into()),
          children: Vec::new(),
        })
      }))
    })
  }

  /// Properties visible on a type after expansion.
  pub fn properties_of(&self, ty: TypeId) -> Vec<PropertyInfo> {
    match self.properties_of_fallible(ty) {
      Ok(props) => props,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  pub fn properties_of_fallible(&self, ty: TypeId) -> Result<Vec<PropertyInfo>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let mut props = queries.properties_of(ty);
      for prop in props.iter_mut() {
        prop.ty = state.prefer_named_refs(prop.ty);
      }
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(props)
    })
  }

  pub fn property_type(&self, ty: TypeId, key: PropertyKey) -> Option<TypeId> {
    match self.property_type_fallible(ty, key) {
      Ok(res) => res,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn property_type_fallible(
    &self,
    ty: TypeId,
    key: PropertyKey,
  ) -> Result<Option<TypeId>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let prop = queries
        .property_type(ty, key)
        .map(|ty| state.prefer_named_refs(ty));
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(prop)
    })
  }

  pub fn call_signatures(&self, ty: TypeId) -> Vec<SignatureInfo> {
    match self.call_signatures_fallible(ty) {
      Ok(sigs) => sigs,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  /// Lookup an interned signature by [`types_ts_interned::SignatureId`].
  pub fn signature(&self, sig: tti::SignatureId) -> Option<tti::Signature> {
    match self.signature_fallible(sig) {
      Ok(sig) => sig,
      Err(fatal) => {
        self.record_fatal(fatal);
        None
      }
    }
  }

  pub fn signature_fallible(&self, sig: tti::SignatureId) -> Result<Option<tti::Signature>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      Ok(store.contains_signature_id(sig).then(|| store.signature(sig)))
    })
  }

  pub fn call_signatures_fallible(&self, ty: TypeId) -> Result<Vec<SignatureInfo>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let sigs = queries.call_signatures(ty);
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(sigs)
    })
  }

  pub fn construct_signatures(&self, ty: TypeId) -> Vec<SignatureInfo> {
    match self.construct_signatures_fallible(ty) {
      Ok(sigs) => sigs,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  pub fn construct_signatures_fallible(
    &self,
    ty: TypeId,
  ) -> Result<Vec<SignatureInfo>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let sigs = queries.construct_signatures(ty);
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(sigs)
    })
  }

  pub fn indexers(&self, ty: TypeId) -> Vec<IndexerInfo> {
    match self.indexers_fallible(ty) {
      Ok(indexers) => indexers,
      Err(fatal) => {
        self.record_fatal(fatal);
        Vec::new()
      }
    }
  }

  pub fn indexers_fallible(&self, ty: TypeId) -> Result<Vec<IndexerInfo>, FatalError> {
    self.with_interned_state(|state| {
      let store = Arc::clone(&state.store);
      let ty = if store.contains_type_id(ty) {
        store.canon(ty)
      } else {
        store.primitive_ids().unknown
      };
      let expander = ProgramTypeExpander {
        def_types: &state.interned_def_types,
        type_params: &state.interned_type_params,
        intrinsics: &state.interned_intrinsics,
      };
      let caches = state.checker_caches.for_body();
      let queries = TypeQueries::with_caches(Arc::clone(&store), &expander, caches.eval.clone());
      let indexers = queries.indexers(ty);
      if matches!(state.compiler_options.cache.mode, CacheMode::PerBody) {
        state.cache_stats.merge(&caches.stats());
      }
      Ok(indexers)
    })
  }
}
