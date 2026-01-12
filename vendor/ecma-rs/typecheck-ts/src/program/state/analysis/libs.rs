use super::*;

impl ProgramState {
  pub(super) fn collect_libraries(
    &mut self,
    host: &dyn Host,
    roots: &[FileKey],
  ) -> Result<Vec<LibFile>, FatalError> {
    let mut options = self
      .compiler_options_override
      .clone()
      .unwrap_or_else(|| host.compiler_options());
    // `strict_native` is a legacy alias for `native_strict`. Treat them as
    // fully synonymous even when only one is explicitly set by the host API.
    let native_strict = options.native_strict || options.strict_native;
    options.native_strict = native_strict;
    options.strict_native = native_strict;
    if !options.no_default_lib && options.libs.is_empty() && !roots.is_empty() {
      for key in roots {
        let text = if let Some(text) = self.file_overrides.get(key) {
          Arc::clone(text)
        } else {
          host.file_text(key)?
        };
        if scan_triple_slash_directives(text.as_ref()).no_default_lib {
          options.no_default_lib = true;
          break;
        }
      }
    }

    let (options, option_diagnostics) = options.normalize_and_validate();
    for diagnostic in option_diagnostics {
      self.push_program_diagnostic(diagnostic);
    }

    if (options.native_strict || options.strict_native) && !options.strict_null_checks {
      let primary = if let Some(key) = roots.first() {
        let file_id = self.intern_file_key(key.clone(), FileOrigin::Source);
        Span::new(file_id, TextRange::new(0, 0))
      } else {
        Span::new(FileId(u32::MAX), TextRange::new(0, 0))
      };
      self.push_program_diagnostic(codes::NATIVE_STRICT_REQUIRES_STRICT_NULL_CHECKS.error(
        "`nativeStrict`/`strictNative` requires `strictNullChecks`; enable `strictNullChecks` (or `strict`) or disable native-strict mode",
        primary,
      ));
    }

    self.compiler_options = options.clone();
    self.checker_caches = CheckerCaches::new(options.cache.clone());
    self.cache_stats = CheckerCacheStats::default();
    self.typecheck_db.set_compiler_options(options.clone());
    self
      .typecheck_db
      .set_cancellation_flag(self.cancelled.clone());
    let store_options = (&options).into();
    if self.store.options() != store_options {
      let store = tti::TypeStore::with_options(store_options);
      self.store = Arc::clone(&store);
      self
        .typecheck_db
        .set_type_store(crate::db::types::SharedTypeStore(Arc::clone(&store)));
      self.decl_types_fingerprint = None;
      self.interned_def_types.clear();
      self.interned_named_def_types.clear();
      self.interned_type_params.clear();
      self.interned_type_param_decls.clear();
      self.interned_intrinsics.clear();
      self.namespace_object_types.clear();
    } else {
      self
        .typecheck_db
        .set_type_store(crate::db::types::SharedTypeStore(Arc::clone(&self.store)));
    }
    let collected = collect_libs(&options, host.lib_files(), &self.lib_manager);
    if collected.files.is_empty() {
      // `no_default_lib` is allowed to suppress all lib loading, but still surface
      // any diagnostics that occurred while resolving explicit `--lib` entries.
      if options.no_default_lib && options.libs.is_empty() && collected.diagnostics.is_empty() {
        self.lib_diagnostics.clear();
        return Ok(Vec::new());
      }
      if collected.diagnostics.is_empty() {
        let validated = validate_libs(Vec::new(), |_| FileId(u32::MAX));
        self.lib_diagnostics = validated.diagnostics;
      } else {
        self.lib_diagnostics = collected.diagnostics;
      }
      return Ok(Vec::new());
    }

    let validated = validate_libs(collected.files, |lib| {
      self.intern_file_key(lib.key.clone(), FileOrigin::Lib)
    });
    let mut lib_diagnostics = collected.diagnostics;
    lib_diagnostics.extend(validated.diagnostics.clone());
    self.lib_diagnostics = lib_diagnostics;

    let mut dts_libs = Vec::new();
    for (lib, file_id) in validated.libs.into_iter() {
      self.file_kinds.insert(file_id, FileKind::Dts);
      dts_libs.push(lib);
    }

    Ok(dts_libs)
  }

  pub(super) fn process_libs(
    &mut self,
    libs: &[LibFile],
    host: &Arc<dyn Host>,
    queue: &mut VecDeque<FileId>,
  ) -> Result<(), FatalError> {
    let mut pending: VecDeque<LibFile> = libs.iter().cloned().collect();
    while let Some(lib) = pending.pop_front() {
      self.check_cancelled()?;
      let file_id = self.intern_file_key(lib.key.clone(), FileOrigin::Lib);
      if self.lib_texts.contains_key(&file_id) {
        continue;
      }
      self.file_kinds.insert(file_id, FileKind::Dts);
      self.lib_texts.insert(file_id, lib.text.clone());

      let directives = scan_triple_slash_directives(lib.text.as_ref());
      let mut triple_slash_types: Vec<&str> = Vec::new();
      for reference in directives.references.iter() {
        let value = reference.value(lib.text.as_ref());
        if value.is_empty() {
          continue;
        }
        match reference.kind {
          TripleSlashReferenceKind::Lib => {
            if let Some(lib_file) =
              crate::lib_support::lib_env::bundled_lib_file_by_option_name(value)
            {
              let lib_id = self.intern_file_key(lib_file.key.clone(), FileOrigin::Lib);
              if !self.lib_texts.contains_key(&lib_id) {
                pending.push_back(lib_file);
              }
            } else {
              self.push_program_diagnostic(codes::LIB_DEFINITION_FILE_NOT_FOUND.error(
                format!("cannot find lib definition file for \"{value}\""),
                Span::new(file_id, reference.value_range),
              ));
            }
          }
          TripleSlashReferenceKind::Path => {
            let normalized = normalize_reference_path_specifier(value);
            if let Some(target) = self.record_module_resolution(file_id, normalized.as_ref(), host)
            {
              queue.push_back(target);
            } else {
              self.push_program_diagnostic(codes::FILE_NOT_FOUND.error(
                format!("file \"{}\" not found", normalized.as_ref()),
                Span::new(file_id, reference.value_range),
              ));
            }
          }
          TripleSlashReferenceKind::Types => {
            triple_slash_types.push(value);
            if let Some(target) = self.record_type_package_resolution(file_id, value, host) {
              queue.push_back(target);
            } else {
              self.push_program_diagnostic(codes::TYPE_DEFINITION_FILE_NOT_FOUND.error(
                format!("cannot find type definition file for \"{value}\""),
                Span::new(file_id, reference.value_range),
              ));
            }
          }
        }
      }

      let parsed = self.parse_via_salsa(file_id, FileKind::Dts, Arc::clone(&lib.text));

      // Keep module resolution edges in sync with the lib's current set of
      // module specifiers, including `@types` fallback behaviour for
      // triple-slash `reference types`.
      let current_specifiers = db::module_specifiers(&self.typecheck_db, file_id);
      let mut keep_specifiers: AHashSet<&str> = AHashSet::new();
      for specifier in current_specifiers.iter() {
        keep_specifiers.insert(specifier.as_ref());
      }
      self
        .typecheck_db
        .retain_module_resolutions_for_file(file_id, |specifier| keep_specifiers.contains(specifier));
      let mut type_package_specifiers: AHashSet<&str> = AHashSet::new();
      for specifier in triple_slash_types.iter().copied() {
        type_package_specifiers.insert(specifier);
      }
      for specifier in current_specifiers.iter() {
        self.check_cancelled()?;
        let specifier = specifier.as_ref();
        let target = if type_package_specifiers.contains(specifier) {
          self.record_type_package_resolution(file_id, specifier, host)
        } else {
          self.record_module_resolution(file_id, specifier, host)
        };
        if let Some(target) = target {
          queue.push_back(target);
        }
      }

      match parsed {
        Ok(ast) => {
          self.check_cancelled()?;
          let (locals, _) = sem_ts::locals::bind_ts_locals_tables(ast.as_ref(), file_id);
          self.local_semantics.insert(file_id, locals);
          self.asts.insert(file_id, Arc::clone(&ast));
          self.queue_type_imports_in_ast(file_id, ast.as_ref(), host, queue);
          let lowered = db::lower_hir(&self.typecheck_db, file_id);
          let Some(lowered) = lowered.lowered else {
            continue;
          };
          self.hir_lowered.insert(file_id, Arc::clone(&lowered));
          let _bound_sem_hir = self.bind_file(file_id, ast.as_ref(), host, queue);
          let _ = self.align_definitions_with_hir(file_id, lowered.as_ref());
          self.map_hir_bodies(file_id, lowered.as_ref());
        }
        Err(err) => {
          let _ = err;
        }
      }
    }
    Ok(())
  }
}
