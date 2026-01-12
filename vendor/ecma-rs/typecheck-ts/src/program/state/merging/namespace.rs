use super::*;

impl ProgramState {
  #[cfg(feature = "serde")]
  pub(in super::super) fn find_namespace_def(&self, file: FileId, name: &str) -> Option<DefId> {
    self
      .def_data
      .iter()
      .find_map(|(id, data)| match &data.kind {
        DefKind::Namespace(_) | DefKind::Module(_) if data.file == file && data.name == name => {
          Some(*id)
        }
        _ => None,
      })
  }

  pub(in super::super) fn merge_namespace_value_types(&mut self) -> Result<(), FatalError> {
    let store = Arc::clone(&self.store);

    #[derive(Default)]
    struct MergeGroup {
      namespace: Option<(TextRange, DefId)>,
      value: Option<(TextRange, DefId)>,
    }

    fn insert_earlier(slot: &mut Option<(TextRange, DefId)>, span: TextRange, def: DefId) {
      match slot {
        None => {
          *slot = Some((span, def));
        }
        Some((existing_span, existing_def)) => {
          if (span.start, span.end, def.0) < (existing_span.start, existing_span.end, existing_def.0) {
            *slot = Some((span, def));
          }
        }
      }
    }

    let mut entries: Vec<_> = self
      .namespace_object_types
      .iter()
      .map(|(k, v)| (k.clone(), *v))
      .collect();
    entries.sort_by(|a, b| (a.0 .0, &a.0 .1).cmp(&(b.0 .0, &b.0 .1)));
    for ((file, name), ns_ty) in entries.into_iter() {
      let Some(lowered) = self.hir_lowered.get(&file).cloned() else {
        continue;
      };

      // `namespace_object_types` is keyed by `(file, name)`, but declaration
      // merging is scoped. Merge types for both:
      // - top-level declarations (parent: None)
      // - declarations inside top-level ambient modules (`declare module "x" { ... }`)
      //
      // This covers common `.d.ts` patterns like exporting a function with a
      // merged namespace in an ambient module, while keeping the legacy
      // file-scope behavior unchanged for other nested declarations.
      let mut groups: HashMap<Option<DefId>, MergeGroup> = HashMap::new();
      for (def_id, data) in self.def_data.iter() {
        if data.file != file || data.name != name {
          continue;
        }

        let (is_namespace, is_value) = match &data.kind {
          DefKind::Namespace(_) | DefKind::Module(_) => (true, false),
          DefKind::Function(_) | DefKind::Class(_) | DefKind::Enum(_) => (false, true),
          _ => continue,
        };

        let Some(mut parent) = lowered.def(*def_id).map(|def| def.parent) else {
          continue;
        };
        while let Some(parent_id) = parent {
          let Some(parent_def) = lowered.def(parent_id) else {
            break;
          };
          if matches!(parent_def.path.kind, HirDefKind::VarDeclarator) {
            parent = parent_def.parent;
            continue;
          }
          break;
        }

        let allowed_scope = match parent {
          None => true,
          Some(parent_id) => lowered
            .def(parent_id)
            .is_some_and(|def| matches!(def.path.kind, HirDefKind::Module) && def.is_ambient),
        };
        if !allowed_scope {
          continue;
        }

        let entry = groups.entry(parent).or_default();
        if is_namespace {
          insert_earlier(&mut entry.namespace, data.span, *def_id);
        } else if is_value {
          insert_earlier(&mut entry.value, data.span, *def_id);
        }
      }

      let mut parents: Vec<_> = groups.keys().copied().collect();
      parents.sort_by_key(|parent| match parent {
        None => (0u8, 0u64),
        Some(def) => (1u8, def.0),
      });

      for parent in parents {
        let Some(group) = groups.get(&parent) else {
          continue;
        };
        let (Some((ns_span, ns_def)), Some((val_span, val_def))) = (group.namespace, group.value)
        else {
          continue;
        };

        let Some(ns_export) = self.def_data.get(&ns_def).map(|data| data.export) else {
          continue;
        };
        let Some(val_export) = self.def_data.get(&val_def).map(|data| data.export) else {
          continue;
        };

        let mut has_error = false;
        if ns_export != val_export {
          // `semantic-js` usually reports TS2395, but we also validate during the
          // type interning/merge pass so `.d.ts` export-mismatch scenarios still
          // surface the expected diagnostics even when the semantic layer does
          // not.
          //
          // Avoid duplicates by only emitting when the semantic phase did not
          // already report a TS2395 for this merged symbol in the same file.
          let needle = format!("'{name}'");
          // TS2652 (default export in a merged declaration) supersedes the
          // TS2395 export mismatch diagnostic in `tsc`; avoid emitting our
          // fallback TS2395 in that case.
          let default_export_reported = self.diagnostics.iter().any(|diag| {
            diag.code.as_str() == "TS2652"
              && diag.primary.file == file
              && diag.message.contains(needle.as_str())
          });
          let already_reported = self.diagnostics.iter().any(|diag| {
            diag.code.as_str() == codes::MERGED_DECLARATIONS_EXPORT_MISMATCH.as_str()
              && diag.primary.file == file
              && diag.message.contains(needle.as_str())
          });
          if !default_export_reported && !already_reported {
            fn refine_name_span(source: &str, decl_span: TextRange, name: &str) -> TextRange {
              if (decl_span.end as usize) <= source.len() {
                if let Some(segment) =
                  source.get(decl_span.start as usize..decl_span.end as usize)
                {
                  if let Some(idx) = segment.find(name) {
                    let start = decl_span.start + idx as u32;
                    let end = start + name.len() as u32;
                    return TextRange::new(start, end);
                  }
                }
              }
              decl_span
            }

            let source = db::file_text(&*self.typecheck_db.lock(), file);
            let ns_name_span = refine_name_span(source.as_ref(), ns_span, &name);
            let val_name_span = refine_name_span(source.as_ref(), val_span, &name);
            let message = format!(
              "Individual declarations in merged declaration '{name}' must be all exported or all local."
            );
            self.push_program_diagnostic(codes::MERGED_DECLARATIONS_EXPORT_MISMATCH.error(
              message.clone(),
              Span::new(file, ns_name_span),
            ));
            self.push_program_diagnostic(codes::MERGED_DECLARATIONS_EXPORT_MISMATCH.error(
              message,
              Span::new(file, val_name_span),
            ));
          }
          has_error = true;
        }

        if ns_span.start < val_span.start {
          // Match tsc: TS2434 is reported by the binder, but the merge continues so
          // namespace members remain visible on the merged value.
        }

        if has_error {
          continue;
        }

        if let Some(val_ty) = self.interned_def_types.get(&val_def).copied() {
          let merged = store.intersection(vec![val_ty, ns_ty]);
          self.interned_def_types.insert(ns_def, merged);
          self.interned_def_types.insert(val_def, merged);
        }
      }
    }
    Ok(())
  }
}
