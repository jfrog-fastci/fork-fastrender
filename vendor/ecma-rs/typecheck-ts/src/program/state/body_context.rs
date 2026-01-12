use super::*;

#[derive(Clone)]
pub(super) struct CachedBodyCheckContext {
  decl_types_fingerprint: u64,
  file_text_revision: u64,
  cache_options: CacheOptions,
  context: Arc<BodyCheckContext>,
}

impl ProgramState {
  pub(super) fn body_check_context(&mut self) -> Arc<BodyCheckContext> {
    let fingerprint = self
      .decl_types_fingerprint
      .unwrap_or_else(|| db::decl_types_fingerprint(&*self.typecheck_db.lock()));
    let file_text_revision = self.file_text_revision;
    let cache_options = self.compiler_options.cache.clone();
    let store = Arc::clone(&self.store);
    if let Some(cached) = self.cached_body_context.as_ref() {
      if cached.decl_types_fingerprint == fingerprint
        && cached.file_text_revision == file_text_revision
        && cached.cache_options == cache_options
        && Arc::ptr_eq(&cached.context.store, &store)
      {
        return Arc::clone(&cached.context);
      }
    }

    let span = QuerySpan::enter(
      QueryKind::BuildBodyContext,
      query_span!(
        "typecheck_ts.build_body_context",
        Option::<u32>::None,
        Option::<u32>::None,
        Option::<u32>::None,
        false
      ),
      None,
      false,
      Some(self.query_stats.clone()),
    );
    self.ensure_def_types_for_body_check_context();
    let context = Arc::new(self.build_body_check_context_snapshot());
    self.cached_body_context = Some(CachedBodyCheckContext {
      decl_types_fingerprint: fingerprint,
      file_text_revision,
      cache_options,
      context: Arc::clone(&context),
    });
    if let Some(span) = span {
      span.finish(None);
    }
    context
  }

  fn ensure_def_types_for_body_check_context(&mut self) {
    let store = Arc::clone(&self.store);
    // `BodyCheckContext` only needs stable, cross-body definition types.
    //
    // `type_of_def` can recursively check bodies (e.g. initializer inference), so
    // inferring types for every definition while the `ProgramState` write lock is
    // held both wastes work (bodies are checked again later) and blocks
    // concurrent read-only queries like `symbol_at`.
    //
    // Restrict inference to definitions that can be referenced from other bodies
    // via global/file bindings and exports. Local bindings are seeded from
    // parent body results instead.
    let mut def_id_set: HashSet<DefId> = HashSet::new();
    for binding in self.global_bindings.values() {
      if let Some(def) = binding.def {
        def_id_set.insert(def);
      }
    }
    for state in self.files.values() {
      for binding in state.bindings.values() {
        if let Some(def) = binding.def {
          def_id_set.insert(def);
        }
      }
      for entry in state.exports.values() {
        if let Some(def) = entry.def {
          def_id_set.insert(def);
        }
      }
    }
    let mut def_ids: Vec<_> = def_id_set.into_iter().collect();
    def_ids.sort_by_key(|def| def.0);
    def_ids.dedup();

    let max_passes = def_ids.len().max(1).min(64);
    for _ in 0..max_passes {
      let mut changed = false;
      for def in def_ids.iter().copied() {
        let needs_type = match self.interned_def_types.get(&def).copied() {
          Some(existing) => {
            matches!(store.type_kind(existing), tti::TypeKind::Unknown)
              || callable_return_is_unknown(&store, existing)
          }
          None => true,
        };
        if !needs_type {
          continue;
        }
        let before = self.interned_def_types.get(&def).copied().map(|t| store.canon(t));
        if let Ok(ty) = self.type_of_def(def) {
          let ty = store.canon(ty);
          self.interned_def_types.insert(def, ty);
          if before != Some(ty) {
            changed = true;
          }
        }
      }
      if !changed {
        break;
      }
    }
  }

  pub(super) fn build_body_check_context_snapshot(&self) -> BodyCheckContext {
    let store = Arc::clone(&self.store);
    let mut body_info = HashMap::new();
    for (id, meta) in self.body_map.iter() {
      body_info.insert(
        *id,
        BodyInfo {
          file: meta.file,
          hir: meta.hir,
          kind: meta.kind,
        },
      );
    }
    let mut file_bindings = HashMap::new();
    for (file, state) in self.files.iter() {
      file_bindings.insert(*file, state.bindings.clone());
    }
    let mut def_spans = HashMap::new();
    for (def, data) in self.def_data.iter() {
      def_spans.insert((data.file, data.span), *def);
    }
    let def_kinds = Arc::new(
      self
        .def_data
        .iter()
        .map(|(id, data)| (*id, data.kind.clone()))
        .collect(),
    );
    let def_files = Arc::new(
      self
        .def_data
        .iter()
        .map(|(id, data)| (*id, data.file))
        .collect(),
    );
    let def_id_spans = Arc::new(
      self
        .def_data
        .iter()
        .map(|(id, data)| (*id, data.span))
        .collect(),
    );
    let exports = Arc::new(
      self
        .files
        .iter()
        .map(|(file, state)| (*file, state.exports.clone()))
        .collect(),
    );
    let namespace_members = self
      .namespace_member_index
      .clone()
      .unwrap_or_else(|| Arc::new(NamespaceMemberIndex::default()));
    BodyCheckContext {
      store: Arc::clone(&store),
      target: self.compiler_options.target,
      no_implicit_any: self.compiler_options.no_implicit_any,
      native_strict: self.compiler_options.native_strict || self.compiler_options.strict_native,
      strict_native: self.compiler_options.strict_native,
      use_define_for_class_fields: self.compiler_options.use_define_for_class_fields,
      interned_def_types: self.interned_def_types.clone(),
      interned_type_params: self.interned_type_params.clone(),
      interned_type_param_decls: self.interned_type_param_decls.clone(),
      interned_intrinsics: self.interned_intrinsics.clone(),
      asts: self.asts.clone(),
      lowered: self
        .hir_lowered
        .iter()
        .map(|(file, lowered)| (*file, Arc::clone(lowered)))
        .collect(),
      body_info,
      body_parents: self.body_parents.clone(),
      global_bindings: self
        .global_bindings
        .iter()
        .map(|(name, binding)| (name.clone(), binding.clone()))
        .collect(),
      file_bindings,
      def_spans,
      semantics: self.semantics.clone(),
      def_kinds,
      def_files,
      def_id_spans,
      exports,
      module_namespace_defs: Arc::new(self.module_namespace_defs.clone()),
      value_defs: Arc::new(self.value_defs.clone()),
      namespace_members,
      qualified_def_members: Arc::clone(&self.qualified_def_members),
      file_registry: Arc::new(self.file_registry.clone()),
      host: Arc::clone(&self.host),
      checker_caches: self.checker_caches.clone(),
      cache_mode: self.compiler_options.cache.mode,
      cache_options: self.compiler_options.cache.clone(),
      jsx_mode: self.compiler_options.jsx,
      jsx_import_source: self.compiler_options.jsx_import_source.clone(),
      query_stats: self.query_stats.clone(),
      cancelled: Arc::clone(&self.cancelled),
    }
  }
}
