use super::*;

impl ProgramState {
  pub(in super::super) fn build_type_resolver(
    &self,
    binding_defs: &HashMap<String, DefId>,
  ) -> Option<Arc<dyn TypeResolver>> {
    if let Some(semantics) = self.semantics.as_ref() {
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
      let def_spans = Arc::new(
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
      let current_file = self.current_file.unwrap_or(FileId(u32::MAX));
      let namespace_members = self
        .namespace_member_index
        .clone()
        .unwrap_or_else(|| Arc::new(NamespaceMemberIndex::default()));
      return Some(Arc::new(ProgramTypeResolver::new(
        Arc::clone(&self.host),
        Arc::clone(semantics),
        def_kinds,
        def_files,
        def_spans,
        Arc::new(self.file_registry.clone()),
        current_file,
        binding_defs.clone(),
        exports,
        Arc::new(self.module_namespace_defs.clone()),
        namespace_members,
        Arc::clone(&self.qualified_def_members),
      )) as Arc<_>);
    }
    if binding_defs.is_empty() {
      return None;
    }
    Some(Arc::new(check::hir_body::BindingTypeResolver::new(
      binding_defs.clone(),
    )) as Arc<_>)
  }
}
