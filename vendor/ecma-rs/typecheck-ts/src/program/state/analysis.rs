use super::*;

mod hir_align;
mod libs;
mod sem_diagnostics;

impl ProgramState {
  /// Map a type package name (as used by `/// <reference types="..." />` and
  /// `compilerOptions.types`) to the corresponding `@types/*` specifier.
  ///
  /// This mirrors TypeScript's scoped package mapping:
  /// - `@scope/pkg` -> `@types/scope__pkg`
  ///
  /// Returning `None` means no fallback should be attempted (e.g. the caller
  /// already specified an `@types/*` package).
  fn type_package_fallback_specifier(specifier: &str) -> Option<String> {
    let (package, rest) = if let Some(stripped) = specifier.strip_prefix('@') {
      let Some((scope, after_scope)) = stripped.split_once('/') else {
        return None;
      };
      if let Some((name, _trailing)) = after_scope.split_once('/') {
        let package_len = 1 + scope.len() + 1 + name.len();
        (&specifier[..package_len], &specifier[package_len..])
      } else {
        (specifier, "")
      }
    } else if let Some((package, _trailing)) = specifier.split_once('/') {
      let package_len = package.len();
      (&specifier[..package_len], &specifier[package_len..])
    } else {
      (specifier, "")
    };

    if package.starts_with("@types/") {
      return None;
    }

    let mapped = if let Some(stripped) = package.strip_prefix('@') {
      let (scope, name) = stripped.split_once('/')?;
      format!("{scope}__{name}")
    } else {
      package.to_string()
    };
    Some(format!("@types/{mapped}{rest}"))
  }

  /// Resolve a type package specifier using normal module resolution with an
  /// additional `@types/*` fallback.
  ///
  /// Policy: `typecheck-ts` core owns type-package resolution for both
  /// `compilerOptions.types` and `/// <reference types="..." />`. Hosts only
  /// need to implement [`Host::resolve`] such that `@types/*` specifiers can be
  /// mapped to the correct `.d.ts` entrypoints (the CLI does this via
  /// `typeRoots`).
  pub(super) fn record_type_package_resolution(
    &mut self,
    from: FileId,
    specifier: &str,
    host: &Arc<dyn Host>,
  ) -> Option<FileId> {
    if let Some(target) = self.record_module_resolution(from, specifier, host) {
      return Some(target);
    }
    let fallback = Self::type_package_fallback_specifier(specifier)?;
    let Some(target) = self.record_module_resolution(from, &fallback, host) else {
      return None;
    };
    // Treat the resolved `@types/*` package as satisfying the original
    // specifier so downstream module graph queries see the dependency.
    self
      .typecheck_db
      .set_module_resolution_ref(from, specifier, Some(target));
    Some(target)
  }

  pub(super) fn ensure_analyzed(&mut self, host: &Arc<dyn Host>, roots: &[FileKey]) {
    if let Err(fatal) = self.ensure_analyzed_result(host, roots) {
      self.diagnostics.push(fatal_to_diagnostic(fatal));
    }
  }

  pub(super) fn ensure_analyzed_result(
    &mut self,
    host: &Arc<dyn Host>,
    roots: &[FileKey],
  ) -> Result<(), FatalError> {
    if self.analyzed {
      return Ok(());
    }
    self.check_cancelled()?;
    self.module_namespace_types.clear();
    self.module_namespace_in_progress.clear();
    let libs = self.collect_libraries(host.as_ref(), roots)?;
    self.check_cancelled()?;
    let mut lib_queue = VecDeque::new();
    self.process_libs(&libs, host, &mut lib_queue)?;

    let mut type_packages = self.compiler_options.types.clone();
    type_packages.sort();
    type_packages.dedup();

    let mut root_keys: Vec<FileKey> = roots.to_vec();
    root_keys.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    root_keys.dedup_by(|a, b| a.as_str() == b.as_str());
    let root_ids_by_key: Vec<FileId> = root_keys
      .iter()
      .map(|key| self.intern_file_key(key.clone(), FileOrigin::Source))
      .collect();
    let mut root_ids: Vec<FileId> = root_ids_by_key.clone();
    root_ids.sort_unstable_by_key(|id| id.0);
    root_ids.dedup();
    self.root_ids = root_ids;
    self
      .typecheck_db
      .set_roots(Arc::<[FileKey]>::from(root_keys));
    let mut queue: VecDeque<FileId> = self.root_ids.iter().copied().collect();
    queue.extend(lib_queue);

    if !type_packages.is_empty() {
      // Match `tsc` by reporting missing `compilerOptions.types` entries as TS2688.
      // These errors are not tied to a specific source location in TypeScript, so
      // use a placeholder span (file-less) for deterministic comparisons.
      let primary = Span::new(FileId(u32::MAX), TextRange::new(0, 0));
      let mut type_package_targets: Vec<FileId> = Vec::new();

      for name in type_packages.iter() {
        self.check_cancelled()?;
        let mut resolved = None;
        for root in root_ids_by_key.iter().copied() {
          if let Some(target) = self.record_type_package_resolution(root, name.as_str(), host) {
            resolved = Some(target);
            break;
          }
        }

        for root in self.root_ids.iter().copied() {
          self
            .typecheck_db
            .set_module_resolution_ref(root, name.as_str(), resolved);
        }

        if let Some(target) = resolved {
          type_package_targets.push(target);
        } else {
          self.push_program_diagnostic(codes::TYPE_DEFINITION_FILE_NOT_FOUND.error(
            format!("cannot find type definition file for \"{name}\""),
            primary,
          ));
        }
      }

      type_package_targets.sort_unstable_by_key(|id| id.0);
      type_package_targets.dedup();
      queue.extend(type_package_targets);
    }

    let mut seen: AHashSet<FileId> = AHashSet::new();
    while let Some(file) = queue.pop_front() {
      self.check_cancelled()?;
      let prev_file = self.current_file;
      self.current_file = Some(file);
      if !seen.insert(file) || self.lib_file_ids.contains(&file) {
        self.current_file = prev_file;
        continue;
      }
      let Some(file_key) = self.file_key_for_id(file) else {
        self.current_file = prev_file;
        continue;
      };
      self
        .file_kinds
        .entry(file)
        .or_insert_with(|| host.file_kind(&file_key));
      let file_kind = *self.file_kinds.get(&file).unwrap_or(&FileKind::Ts);
      let text = self.load_text(file, host)?;
      self.check_cancelled()?;
      let directives = scan_triple_slash_directives(text.as_ref());
      let mut triple_slash_types: Vec<&str> = Vec::new();
      for reference in directives.references.iter() {
        let value = reference.value(text.as_ref());
        if value.is_empty() {
          continue;
        }
        match reference.kind {
          TripleSlashReferenceKind::Lib => {
            if let Some(lib_file) =
              crate::lib_support::lib_env::bundled_lib_file_by_option_name(value)
            {
              self.process_libs(std::slice::from_ref(&lib_file), host, &mut queue)?;
            } else {
              self.push_program_diagnostic(codes::LIB_DEFINITION_FILE_NOT_FOUND.error(
                format!("cannot find lib definition file for \"{value}\""),
                Span::new(file, reference.value_range),
              ));
            }
          }
          TripleSlashReferenceKind::Path => {
            let normalized = normalize_reference_path_specifier(value);
            if let Some(target) = self.record_module_resolution(file, normalized.as_ref(), host) {
              queue.push_back(target);
            } else {
              self.push_program_diagnostic(codes::FILE_NOT_FOUND.error(
                format!("file \"{}\" not found", normalized.as_ref()),
                Span::new(file, reference.value_range),
              ));
            }
          }
          TripleSlashReferenceKind::Types => {
            triple_slash_types.push(value);
            if let Some(target) = self.record_type_package_resolution(file, value, host) {
              queue.push_back(target);
            } else {
              self.push_program_diagnostic(codes::TYPE_DEFINITION_FILE_NOT_FOUND.error(
                format!("cannot find type definition file for \"{value}\""),
                Span::new(file, reference.value_range),
              ));
            }
          }
        }
      }
      let parse_span = QuerySpan::enter(
        QueryKind::Parse,
        query_span!(
          "typecheck_ts.parse",
          Some(file.0),
          Option::<u32>::None,
          Option::<u32>::None,
          false
        ),
        None,
        false,
        Some(self.query_stats.clone()),
      );
      let parsed = self.parse_via_salsa(file, file_kind, Arc::clone(&text));
      if let Some(span) = parse_span {
        span.finish(None);
      }
      self.check_cancelled()?;

      // Keep the host module resolution edges in sync with the current set of
      // module specifiers in the file. This avoids accumulating stale edges
      // once program edits become incremental (without recreating the salsa DB)
      // and keeps serialized snapshots consistent with the current module graph.
      let current_specifiers = db::module_specifiers(&self.typecheck_db, file);
      let is_root = self.root_ids.contains(&file);
      let mut keep_specifiers: AHashSet<&str> = AHashSet::new();
      for specifier in current_specifiers.iter() {
        keep_specifiers.insert(specifier.as_ref());
      }
      if is_root {
        for specifier in type_packages.iter() {
          keep_specifiers.insert(specifier.as_str());
        }
      }
      self
        .typecheck_db
        .retain_module_resolutions_for_file(file, |specifier| keep_specifiers.contains(specifier));

      let mut type_package_specifiers: AHashSet<&str> = AHashSet::new();
      for specifier in triple_slash_types.iter().copied() {
        type_package_specifiers.insert(specifier);
      }

      for specifier in current_specifiers.iter() {
        self.check_cancelled()?;
        let specifier = specifier.as_ref();
        let target = if type_package_specifiers.contains(specifier) {
          self.record_type_package_resolution(file, specifier, host)
        } else {
          self.record_module_resolution(file, specifier, host)
        };
        if let Some(target) = target {
          queue.push_back(target);
        }
      }

      match parsed {
        Ok(ast) => {
          let (locals, _) = sem_ts::locals::bind_ts_locals_tables(ast.as_ref(), file);
          self.local_semantics.insert(file, locals);
          self.asts.insert(file, Arc::clone(&ast));
          self.queue_type_imports_in_ast(file, ast.as_ref(), host, &mut queue);
          let lower_span = QuerySpan::enter(
            QueryKind::LowerHir,
            query_span!(
              "typecheck_ts.lower_hir",
              Some(file.0),
              Option::<u32>::None,
              Option::<u32>::None,
              false
            ),
            None,
            false,
            Some(self.query_stats.clone()),
          );
          let lowered = db::lower_hir(&self.typecheck_db, file);
          let Some(lowered) = lowered.lowered else {
            if let Some(span) = lower_span {
              span.finish(None);
            }
            continue;
          };
          self.hir_lowered.insert(file, Arc::clone(&lowered));
          if (self.compiler_options.native_strict || self.compiler_options.strict_native)
            && !matches!(file_kind, FileKind::Dts)
          {
            for arenas in lowered.types.values() {
              for ty_expr in arenas.type_exprs.iter() {
                if !matches!(ty_expr.kind, hir_js::TypeExprKind::Any) {
                  continue;
                }
                self.push_program_diagnostic(codes::NATIVE_STRICT_ANY.error(
                  "`any` is forbidden when `native_strict` is enabled",
                  Span::new(file, ty_expr.span),
                ));
              }
            }
          }
          let _bound_sem_hir = self.bind_file(file, ast.as_ref(), host, &mut queue);
          let _ = self.align_definitions_with_hir(file, lowered.as_ref());
          self.map_hir_bodies(file, lowered.as_ref());
          self.check_cancelled()?;
          if let Some(span) = lower_span {
            span.finish(None);
          }
        }
        Err(err) => {
          let _ = err;
        }
      }
      self.current_file = prev_file;
    }
    if !self.hir_lowered.is_empty() {
      self.check_cancelled()?;
      let ts_semantics = db::ts_semantics(&self.typecheck_db);
      self.check_cancelled()?;
      self.semantics = Some(Arc::clone(&ts_semantics.semantics));
      self.extend_symbol_to_def_with_semantic_ids();
      self.push_semantic_diagnostics(ts_semantics.diagnostics.as_ref().clone());
      self.check_export_assignments_in_esm();
      self.check_import_assignment_requires();
      self.check_required_global_types();
    }
    self.check_cancelled()?;
    self.resolve_reexports();
    self.rebuild_callable_overloads();
    self.rebuild_module_namespace_defs();
    self.recompute_global_bindings();
    self.rebuild_namespace_member_index()?;
    self.rebuild_body_owners();
    self.analyzed = true;
    Ok(())
  }
}
