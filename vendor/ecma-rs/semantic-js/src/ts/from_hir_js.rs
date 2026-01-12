use crate::ts::module_syntax::ast_has_module_syntax;
use crate::ts::{
  AmbientModule, Decl, DeclKind, Export, ExportAll, ExportAsNamespace, ExportSpecifier, Exported,
  FileKind, HirFile, Import, ImportDefault, ImportEquals, ImportEqualsTarget, ImportNamed,
  ImportNamespace, ModuleKind, NamedExport, TypeImport as TsTypeImport, VarKind,
};
use diagnostics::TextRange;
use hir_js::{DefId, DefKind, ExportKind, FileKind as HirFileKind, ImportKind, LowerResult};
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::Expr;
use parse_js::ast::import_export::{ExportNames, ImportNames};
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stx::TopLevel;
use parse_js::ast::ts_stmt::{ImportEqualsRhs, ModuleName};
use parse_js::ast::stmt::decl::VarDeclMode;
use parse_js::loc::Loc;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Convert a lowered `hir-js` file into the `semantic-js` TS binder input model.
///
/// This adapter preserves `DefId`s from `hir-js` so downstream semantics and type
/// checking can correlate binder declarations with lowered HIR data without
/// renumbering.
pub fn lower_to_ts_hir(ast: &Node<TopLevel>, lower: &LowerResult, source: &str) -> HirFile {
  let file_id = lower.hir.file;
  let item_ids: HashSet<DefId> = lower.hir.items.iter().copied().collect();

  let imports_by_span: HashMap<_, _> = lower
    .hir
    .imports
    .iter()
    .map(|import| (import.span, import))
    .collect();
  let exports_by_span: HashMap<_, _> = lower
    .hir
    .exports
    .iter()
    .map(|export| (export.span, export))
    .collect();
  let import_specifier_span = |range: TextRange| -> Option<TextRange> {
    imports_by_span
      .get(&range)
      .and_then(|import| match &import.kind {
        ImportKind::Es(es) => Some(es.specifier.span),
        ImportKind::ImportEquals(eq) => match &eq.target {
          hir_js::ImportEqualsTarget::Module(spec) => Some(spec.span),
          _ => None,
        },
      })
  };
  let export_specifier_span = |range: TextRange| -> Option<TextRange> {
    exports_by_span
      .get(&range)
      .and_then(|export| match &export.kind {
        ExportKind::Named(named) => named.source.as_ref().map(|s| s.span),
        ExportKind::ExportAll(all) => Some(all.source.span),
        _ => None,
      })
  };

  let module_kind = if ast_has_module_syntax(ast) {
    ModuleKind::Module
  } else {
    ModuleKind::Script
  };

  let block = lower_block(
    &ast.stx.body,
    lower,
    module_kind,
    Some(&item_ids),
    &import_specifier_span,
    &export_specifier_span,
    source,
  );

  let finalized = finalize_block(block, lower, module_kind, source);
  let type_imports = collect_type_imports(lower);

  HirFile {
    file_id,
    module_kind,
    file_kind: match lower.hir.file_kind {
      HirFileKind::Dts => FileKind::Dts,
      _ => FileKind::Ts,
    },
    decls: finalized.decls,
    imports: finalized.imports,
    type_imports,
    import_equals: finalized.import_equals,
    exports: finalized.exports,
    export_as_namespace: finalized.export_as_namespace,
    ambient_modules: finalized.ambient_modules,
  }
}

fn collect_type_imports(lower: &LowerResult) -> Vec<TsTypeImport> {
  let mut seen = BTreeSet::<(String, TextRange)>::new();
  for arenas in lower.types.values() {
    for ty in arenas.type_exprs.iter() {
      match &ty.kind {
        hir_js::hir::TypeExprKind::TypeRef(type_ref) => {
          if let hir_js::hir::TypeName::Import(import) = &type_ref.name {
            if let Some(module) = &import.module {
              seen.insert((module.clone(), ty.span));
            }
          }
        }
        hir_js::hir::TypeExprKind::TypeQuery(name) => {
          if let hir_js::hir::TypeName::Import(import) = name {
            if let Some(module) = &import.module {
              seen.insert((module.clone(), ty.span));
            }
          }
        }
        hir_js::hir::TypeExprKind::Import(import) => {
          seen.insert((import.module.clone(), ty.span));
        }
        _ => {}
      }
    }
  }
  seen
    .into_iter()
    .map(|(specifier, specifier_span)| TsTypeImport {
      specifier,
      specifier_span,
    })
    .collect()
}

struct BlockResult {
  local_defs: Vec<DefId>,
  exported: HashMap<DefId, Exported>,
  var_kinds: HashMap<DefId, VarKind>,
  imports: Vec<Import>,
  import_equals: Vec<ImportEquals>,
  exports: Vec<Export>,
  export_as_namespace: Vec<ExportAsNamespace>,
  ambient_modules: Vec<AmbientModule>,
}

struct LoweredBlock {
  decls: Vec<Decl>,
  imports: Vec<Import>,
  import_equals: Vec<ImportEquals>,
  exports: Vec<Export>,
  export_as_namespace: Vec<ExportAsNamespace>,
  ambient_modules: Vec<AmbientModule>,
}

fn lower_block(
  stmts: &[Node<Stmt>],
  lower: &LowerResult,
  outer_module_kind: ModuleKind,
  allowed_defs: Option<&HashSet<DefId>>,
  import_specifier_span: &impl Fn(TextRange) -> Option<TextRange>,
  export_specifier_span: &impl Fn(TextRange) -> Option<TextRange>,
  source: &str,
) -> BlockResult {
  let targets = collect_def_targets(stmts);
  let local_defs = resolve_def_targets(&targets, lower, allowed_defs);
  let defs_by_name = build_name_map(&local_defs, lower);

  let mut result = BlockResult {
    local_defs,
    exported: HashMap::new(),
    var_kinds: HashMap::new(),
    imports: Vec::new(),
    import_equals: Vec::new(),
    exports: Vec::new(),
    export_as_namespace: Vec::new(),
    ambient_modules: Vec::new(),
  };

  for stmt in stmts.iter() {
    let stmt_range = to_range(stmt.loc);
    match stmt.stx.as_ref() {
      Stmt::Import(import) => {
        let mut default = None;
        if let Some(pat) = import.stx.default.as_ref() {
          if let Some(name) = pat_name(&pat.stx.pat) {
            default = Some(ImportDefault {
              local_span: to_range(pat.loc),
              local: name,
              is_type_only: import.stx.type_only,
            });
          }
        }

        let mut namespace = None;
        let mut named = Vec::new();
        match &import.stx.names {
          Some(ImportNames::All(pat)) => {
            if let Some(name) = pat_name(&pat.stx.pat) {
              namespace = Some(ImportNamespace {
                local: name,
                local_span: to_range(pat.loc),
                is_type_only: import.stx.type_only,
              });
            }
          }
          Some(ImportNames::Specific(list)) => {
            for entry in list {
              if let Some(local) = pat_name(&entry.stx.alias.stx.pat) {
                named.push(ImportNamed {
                  imported: entry.stx.importable.as_str().to_string(),
                  local,
                  is_type_only: import.stx.type_only || entry.stx.type_only,
                  imported_span: to_range(entry.loc),
                  local_span: to_range(entry.stx.alias.loc),
                });
              }
            }
          }
          None => {}
        }

        result.imports.push(Import {
          specifier: import.stx.module.clone(),
          specifier_span: import_specifier_span(stmt_range).unwrap_or(stmt_range),
          default,
          namespace,
          named,
          is_type_only: import.stx.type_only,
        });
      }
      Stmt::ImportEqualsDecl(import_eq) => {
        let target = match &import_eq.stx.rhs {
          ImportEqualsRhs::Require { module } => ImportEqualsTarget::Require {
            specifier: module.clone(),
            specifier_span: import_specifier_span(stmt_range).unwrap_or(stmt_range),
          },
          ImportEqualsRhs::EntityName { path } => ImportEqualsTarget::EntityName {
            path: path.clone(),
            span: stmt_range,
          },
        };

        if import_eq.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::ImportBinding),
            Exported::Named,
            &mut result.exported,
          );
        }

        result.import_equals.push(ImportEquals {
          local: import_eq.stx.name.clone(),
          local_span: span_for_name(stmt.loc, &import_eq.stx.name),
          target,
          is_exported: import_eq.stx.export,
        });
      }
      Stmt::ExportList(list) => match &list.stx.names {
        ExportNames::All(alias) => {
          if let Some(specifier) = list.stx.from.clone() {
            result.exports.push(Export::All(ExportAll {
              specifier_span: export_specifier_span(stmt_range).unwrap_or(stmt_range),
              specifier,
              is_type_only: list.stx.type_only,
              alias: alias.as_ref().map(|a| a.stx.name.clone()),
              alias_span: alias.as_ref().map(|a| to_range(a.loc)),
            }));
          }
        }
        ExportNames::Specific(names) => {
          let mut items = Vec::new();
          for name in names {
            let local = name.stx.exportable.as_str().to_string();
            let exported_name = name.stx.alias.stx.name.clone();
            let exported_span = if exported_name == local {
              None
            } else {
              Some(to_range(name.stx.alias.loc))
            };
            let is_type_only = list.stx.type_only || name.stx.type_only;
            items.push(ExportSpecifier {
              local: local.clone(),
              exported: if exported_span.is_some() {
                Some(exported_name)
              } else {
                None
              },
              is_type_only,
              local_span: to_range(name.loc),
              exported_span,
            });
            if list.stx.from.is_none() && !is_type_only {
              let export_kind =
                if exported_span.is_some() && name.stx.alias.stx.name == "default" {
                  Exported::Default
                } else {
                  Exported::Named
                };
              mark_defs_with_name(
                &defs_by_name,
                &local,
                export_kind.clone(),
                &mut result.exported,
              );
            }
          }
          result.exports.push(Export::Named(NamedExport {
            specifier: list.stx.from.clone(),
            specifier_span: list
              .stx
              .from
              .as_ref()
              .map(|_| export_specifier_span(stmt_range).unwrap_or(stmt_range)),
            items,
            is_type_only: list.stx.type_only,
          }));
        }
      },
      Stmt::ExportDefaultExpr(expr) => {
        // `export default foo;` contributes an exported declaration for `foo`
        // (not a separate `default` binding), which affects merge diagnostics
        // like TS2395. If the exported expression is a simple identifier, mark
        // the referenced value declaration(s) as default-exported.
        //
        // Otherwise, fall back to tracking the synthetic `ExportAlias`
        // definition so the module still has a default export.
        let exported_path = entity_name_path(&expr.stx.expression);
        let mut handled = false;
        if let Some(path) = exported_path {
          if path.len() == 1 {
            let name = &path[0];
            if let Some(defs) = defs_by_name.get(name) {
              for def_id in defs {
                let def = def_by_id(*def_id, &lower.defs, &lower.def_index);
                match def.path.kind {
                  DefKind::Function | DefKind::Class | DefKind::Var | DefKind::Enum | DefKind::ImportBinding => {
                    result
                      .exported
                      .entry(*def_id)
                      .or_insert(Exported::Default);
                    handled = true;
                  }
                  _ => {}
                }
              }
            }
          }
        }
        if !handled {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::ExportAlias),
            Exported::Default,
            &mut result.exported,
          );
        }
      }
      Stmt::ExportAssignmentDecl(assign) => {
        let expr_span = to_range(assign.stx.expression.loc);
        let path = entity_name_path(&assign.stx.expression);
        result.exports.push(Export::ExportAssignment {
          path,
          expr_span,
          span: stmt_range,
        });
      }
      Stmt::ExportAsNamespaceDecl(decl) => {
        result.export_as_namespace.push(ExportAsNamespace {
          name: decl.stx.name.clone(),
          span: stmt_range,
        });
      }
      Stmt::ImportTypeDecl(import_type) => {
        let named = import_type
          .stx
          .names
          .iter()
          .map(|n| ImportNamed {
            imported: n.imported.clone(),
            local: n.local.clone().unwrap_or_else(|| n.imported.clone()),
            is_type_only: true,
            imported_span: stmt_range,
            local_span: stmt_range,
          })
          .collect();
        result.imports.push(Import {
          specifier: import_type.stx.module.clone(),
          specifier_span: import_specifier_span(stmt_range).unwrap_or(stmt_range),
          default: None,
          namespace: None,
          named,
          is_type_only: true,
        });
      }
      Stmt::ExportTypeDecl(export_type) => {
        let items = export_type
          .stx
          .names
          .iter()
          .map(|n| ExportSpecifier {
            local: n.local.clone(),
            exported: n.exported.clone(),
            is_type_only: true,
            local_span: stmt_range,
            exported_span: n.exported.as_ref().map(|_| stmt_range),
          })
          .collect();
        result.exports.push(Export::Named(NamedExport {
          specifier: export_type.stx.module.clone(),
          specifier_span: export_type
            .stx
            .module
            .as_ref()
            .map(|_| export_specifier_span(stmt_range).unwrap_or(stmt_range)),
          items,
          is_type_only: true,
        }));
      }
      Stmt::VarDecl(var) => {
        let kind = match var.stx.mode {
          VarDeclMode::Var => VarKind::Var,
          VarDeclMode::Let => VarKind::Let,
          VarDeclMode::Const => VarKind::Const,
          VarDeclMode::Using => VarKind::Using,
          VarDeclMode::AwaitUsing => VarKind::AwaitUsing,
        };
        mark_var_kind_in_span(
          &result.local_defs,
          lower,
          stmt_range,
          Some(DefKind::Var),
          kind,
          &mut result.var_kinds,
        );
        if var.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Var),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::FunctionDecl(func) => {
        if func.stx.export || func.stx.export_default {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Function),
            if func.stx.export_default {
              Exported::Default
            } else {
              Exported::Named
            },
            &mut result.exported,
          );
        }
      }
      Stmt::ClassDecl(class_decl) => {
        if class_decl.stx.export || class_decl.stx.export_default {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Class),
            if class_decl.stx.export_default {
              Exported::Default
            } else {
              Exported::Named
            },
            &mut result.exported,
          );
        }
      }
      Stmt::InterfaceDecl(intf) => {
        if intf.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Interface),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::TypeAliasDecl(alias) => {
        if alias.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::TypeAlias),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::EnumDecl(en) => {
        if en.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Enum),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::NamespaceDecl(ns) => {
        if ns.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Namespace),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::ModuleDecl(module) => match &module.stx.name {
        ModuleName::Identifier(_) => {
          if module.stx.export {
            mark_defs_in_span(
              &result.local_defs,
              lower,
              stmt_range,
              Some(DefKind::Module),
              Exported::Named,
              &mut result.exported,
            );
          }
        }
        ModuleName::String(spec) => {
          let name_span = to_range(module.stx.name_loc);
          let export_modifier = module.stx.export;
          let export_modifier_span = module
            .stx
            .export
            .then(|| export_modifier_span(source, stmt_range, name_span))
            .flatten();
          let nested = lower_block(
            module.stx.body.as_deref().unwrap_or(&[]),
            lower,
            outer_module_kind,
            None,
            import_specifier_span,
            export_specifier_span,
            source,
          );
          let nested = finalize_block(nested, lower, outer_module_kind, source);
          result.ambient_modules.push(AmbientModule {
            name: spec.clone(),
            name_span,
            export_modifier,
            export_modifier_span,
            decls: nested.decls,
            imports: nested.imports,
            type_imports: Vec::new(),
            import_equals: nested.import_equals,
            exports: nested.exports,
            export_as_namespace: nested.export_as_namespace,
            ambient_modules: nested.ambient_modules,
          });
        }
      },
      Stmt::GlobalDecl(global) => {
        let nested = lower_block(
          &global.stx.body,
          lower,
          outer_module_kind,
          allowed_defs,
          import_specifier_span,
          export_specifier_span,
          source,
        );
        result.local_defs.extend(nested.local_defs);
        result.exported.extend(nested.exported);
        result.var_kinds.extend(nested.var_kinds);
        result.imports.extend(nested.imports);
        result.import_equals.extend(nested.import_equals);
        result.exports.extend(nested.exports);
        result
          .export_as_namespace
          .extend(nested.export_as_namespace);
        result.ambient_modules.extend(nested.ambient_modules);
      }
      Stmt::AmbientVarDecl(av) => {
        mark_var_kind_in_span(
          &result.local_defs,
          lower,
          stmt_range,
          Some(DefKind::Var),
          VarKind::Var,
          &mut result.var_kinds,
        );
        if av.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Var),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::AmbientFunctionDecl(af) => {
        if af.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Function),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      Stmt::AmbientClassDecl(ac) => {
        if ac.stx.export {
          mark_defs_in_span(
            &result.local_defs,
            lower,
            stmt_range,
            Some(DefKind::Class),
            Exported::Named,
            &mut result.exported,
          );
        }
      }
      _ => {}
    }
  }

  result
}

fn finalize_block(
  block: BlockResult,
  lower: &LowerResult,
  _module_kind: ModuleKind,
  source: &str,
) -> LoweredBlock {
  let BlockResult {
    local_defs,
    exported,
    var_kinds,
    imports,
    import_equals,
    exports,
    export_as_namespace,
    ambient_modules,
  } = block;

  let exported_map = exported;

  let mut decls = Vec::new();
  for def_id in local_defs.iter().copied() {
    let def = def_by_id(def_id, &lower.defs, &lower.def_index);
    let kind = match def.path.kind {
      DefKind::Function => Some(DeclKind::Function),
      DefKind::Class => Some(DeclKind::Class),
      DefKind::Var => Some(DeclKind::Var),
      DefKind::Interface => Some(DeclKind::Interface),
      DefKind::TypeAlias => Some(DeclKind::TypeAlias),
      DefKind::Enum => Some(DeclKind::Enum),
      DefKind::Namespace | DefKind::Module => Some(DeclKind::Namespace),
      DefKind::ImportBinding => Some(DeclKind::ImportBinding),
      DefKind::ExportAlias => {
        if exported_map.get(&def_id) == Some(&Exported::Default) {
          Some(DeclKind::Var)
        } else {
          None
        }
      }
      _ => None,
    };
    if let Some(kind) = kind {
      let name = lower
        .names
        .resolve(def.name)
        .unwrap_or("<anon>")
        .to_string();
      let var_kind = match kind {
        DeclKind::Var => Some(var_kinds.get(&def_id).copied().unwrap_or(VarKind::Var)),
        _ => None,
      };
      let exported = exported_map.get(&def_id).cloned().unwrap_or(Exported::No);
      let name_span = find_name_span(source, &name, def.span);
      decls.push(Decl {
        def_id,
        name,
        kind,
        var_kind,
        is_ambient: def.is_ambient,
        is_global: def.in_global,
        exported,
        span: def.span,
        name_span,
      });
    }
  }

  LoweredBlock {
    decls,
    imports,
    import_equals,
    exports,
    export_as_namespace,
    ambient_modules,
  }
}

fn is_ident_char(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn find_name_span(source: &str, name: &str, range: TextRange) -> TextRange {
  if name.is_empty() || name == "<anon>" {
    return range;
  }

  let bytes = source.as_bytes();
  let start = (range.start as usize).min(bytes.len());
  let end = (range.end as usize).min(bytes.len());
  let slice = &source[start..end];
  let mut offset = 0usize;
  while offset <= slice.len() {
    let Some(pos) = slice[offset..].find(name) else {
      break;
    };
    let abs_start = start + offset + pos;
    let abs_end = abs_start + name.len();
    if abs_end > bytes.len() {
      break;
    }
    let before_ok = abs_start == 0 || !is_ident_char(bytes[abs_start - 1]);
    let after_ok = abs_end == bytes.len() || !is_ident_char(bytes[abs_end]);
    if before_ok && after_ok {
      return TextRange::new(abs_start as u32, abs_end as u32);
    }
    offset = offset.saturating_add(pos.saturating_add(name.len().max(1)));
  }
  range
}

#[derive(Clone, Copy)]
struct DefTarget {
  span: TextRange,
  kind: DefKind,
}

fn collect_def_targets(stmts: &[Node<Stmt>]) -> Vec<DefTarget> {
  let mut targets = Vec::new();
  for stmt in stmts {
    let span = to_range(stmt.loc);
    match stmt.stx.as_ref() {
      Stmt::Import(import) => {
        if let Some(default) = &import.stx.default {
          targets.push(DefTarget {
            span: to_range(default.loc),
            kind: DefKind::ImportBinding,
          });
        }
        match &import.stx.names {
          Some(ImportNames::All(pat)) => targets.push(DefTarget {
            span: to_range(pat.loc),
            kind: DefKind::ImportBinding,
          }),
          Some(ImportNames::Specific(list)) => {
            for entry in list {
              targets.push(DefTarget {
                span: to_range(entry.stx.alias.loc),
                kind: DefKind::ImportBinding,
              });
            }
          }
          None => {}
        }
      }
      Stmt::VarDecl(var) => {
        for decl in var.stx.declarators.iter() {
          targets.push(DefTarget {
            span: to_range(decl.pattern.loc),
            kind: DefKind::Var,
          });
        }
      }
      Stmt::FunctionDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Function,
      }),
      Stmt::ClassDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Class,
      }),
      Stmt::InterfaceDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Interface,
      }),
      Stmt::TypeAliasDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::TypeAlias,
      }),
      Stmt::EnumDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Enum,
      }),
      Stmt::NamespaceDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Namespace,
      }),
      Stmt::ModuleDecl(module) => {
        if matches!(module.stx.name, ModuleName::Identifier(_)) {
          targets.push(DefTarget {
            span,
            kind: DefKind::Module,
          });
        }
      }
      Stmt::AmbientVarDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Var,
      }),
      Stmt::AmbientFunctionDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Function,
      }),
      Stmt::AmbientClassDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::Class,
      }),
      Stmt::GlobalDecl(global) => targets.extend(collect_def_targets(&global.stx.body)),
      Stmt::ExportDefaultExpr(_) => targets.push(DefTarget {
        span,
        kind: DefKind::ExportAlias,
      }),
      Stmt::ImportEqualsDecl(_) => targets.push(DefTarget {
        span,
        kind: DefKind::ImportBinding,
      }),
      _ => {}
    }
  }
  targets
}

fn resolve_def_targets(
  targets: &[DefTarget],
  lower: &LowerResult,
  allowed_defs: Option<&HashSet<DefId>>,
) -> Vec<DefId> {
  let mut selected = Vec::new();
  let mut seen = HashSet::new();
  for def in &lower.defs {
    if let Some(allowed) = allowed_defs {
      if !allowed.contains(&def.id) {
        continue;
      }
    }
    if targets
      .iter()
      .any(|target| target.span == def.span && target.kind == def.path.kind)
    {
      if seen.insert(def.id) {
        selected.push(def.id);
      }
    }
  }
  selected
}

fn build_name_map(local_defs: &[DefId], lower: &LowerResult) -> HashMap<String, Vec<DefId>> {
  let mut names: HashMap<String, Vec<DefId>> = HashMap::new();
  for def_id in local_defs {
    let def = def_by_id(*def_id, &lower.defs, &lower.def_index);
    if let Some(name) = lower.names.resolve(def.name) {
      names.entry(name.to_string()).or_default().push(*def_id);
    }
  }
  names
}

fn def_by_id<'a>(
  def_id: DefId,
  defs: &'a [hir_js::DefData],
  def_index: &BTreeMap<DefId, usize>,
) -> &'a hir_js::DefData {
  let idx = def_index
    .get(&def_id)
    .copied()
    .expect("def present in index");
  &defs[idx]
}

fn mark_defs_with_name(
  available_defs: &HashMap<String, Vec<DefId>>,
  name: &str,
  exported: Exported,
  out: &mut HashMap<DefId, Exported>,
) {
  if let Some(defs) = available_defs.get(name) {
    for def_id in defs {
      out.entry(*def_id).or_insert(exported.clone());
    }
  }
}

fn mark_defs_in_span(
  local_defs: &[DefId],
  lower: &LowerResult,
  span: TextRange,
  kind: Option<DefKind>,
  exported: Exported,
  out: &mut HashMap<DefId, Exported>,
) {
  for def_id in local_defs.iter().copied() {
    let def = def_by_id(def_id, &lower.defs, &lower.def_index);
    if def.span.start >= span.start && def.span.end <= span.end {
      if let Some(k) = kind {
        if def.path.kind != k {
          continue;
        }
      }
      out.entry(def_id).or_insert(exported.clone());
    }
  }
}

fn mark_var_kind_in_span(
  local_defs: &[DefId],
  lower: &LowerResult,
  span: TextRange,
  kind: Option<DefKind>,
  var_kind: VarKind,
  out: &mut HashMap<DefId, VarKind>,
) {
  for def_id in local_defs.iter().copied() {
    let def = def_by_id(def_id, &lower.defs, &lower.def_index);
    if def.span.start >= span.start && def.span.end <= span.end {
      if let Some(k) = kind {
        if def.path.kind != k {
          continue;
        }
      }
      out.entry(def_id).or_insert(var_kind);
    }
  }
}

fn pat_name(pat: &Node<Pat>) -> Option<String> {
  match pat.stx.as_ref() {
    Pat::Id(id) => Some(id.stx.name.clone()),
    _ => None,
  }
}

fn entity_name_path(expr: &Node<Expr>) -> Option<Vec<String>> {
  match expr.stx.as_ref() {
    Expr::Id(id) => Some(vec![id.stx.name.clone()]),
    Expr::Member(member) if !member.stx.optional_chaining => {
      let mut path = entity_name_path(&member.stx.left)?;
      path.push(member.stx.right.clone());
      Some(path)
    }
    _ => None,
  }
}

fn to_range(loc: Loc) -> TextRange {
  TextRange::new(loc.start_u32(), loc.end_u32())
}

fn span_for_name(loc: Loc, name: &str) -> TextRange {
  let range = to_range(loc);
  let len = name.len() as u32;
  if range.len() >= len {
    return range;
  }
  let end = range.start;
  let start = end.saturating_sub(len);
  TextRange::new(start, end)
}

fn export_modifier_span(
  source: &str,
  stmt_span: TextRange,
  name_span: TextRange,
) -> Option<TextRange> {
  if stmt_span.is_empty() {
    return None;
  }

  // Keep the search window before the module name so we don't accidentally pick
  // up `export` keywords from inside the module body.
  //
  // Note: `parse-js` statement spans for `module "..."` declarations do not
  // always include leading modifiers (`export`/`declare`). We therefore scan a
  // small lookbehind window to recover a stable `export` token span.
  let search_end = std::cmp::min(stmt_span.end, name_span.start);
  let search_start = stmt_span.start.saturating_sub(64);
  let search_span = TextRange::new(search_start, search_end);
  if let Some(found) = find_token_outside_strings_and_comments(source, search_span, b"export") {
    return Some(found);
  }

  // Fall back to the start of the statement to avoid losing the fact that the
  // declaration was parsed as `export ...`. This is best-effort and should only
  // trigger when the slice is out of bounds or contains unexpected trivia.
  let start = std::cmp::min(stmt_span.start, search_end);
  let end = std::cmp::min(search_end, start.saturating_add(6));
  Some(TextRange::new(start, end))
}

fn find_token_outside_strings_and_comments(
  source: &str,
  span: TextRange,
  token: &[u8],
) -> Option<TextRange> {
  let bytes = source.as_bytes();
  let start = span.start as usize;
  let mut end = span.end as usize;
  if start >= bytes.len() {
    return None;
  }
  end = std::cmp::min(end, bytes.len());
  if start >= end || token.is_empty() || token.len() > end - start {
    return None;
  }

  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum State {
    Code,
    LineComment,
    BlockComment,
    SingleString,
    DoubleString,
    TemplateString,
  }

  fn is_ident_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
  }

  let mut state = State::Code;
  let mut found = None;
  let mut i = start;
  while i < end {
    match state {
      State::Code => {
        if bytes[i] == b'/' && i + 1 < end {
          match bytes[i + 1] {
            b'/' => {
              state = State::LineComment;
              i += 2;
              continue;
            }
            b'*' => {
              state = State::BlockComment;
              i += 2;
              continue;
            }
            _ => {}
          }
        }

        match bytes[i] {
          b'\'' => {
            state = State::SingleString;
            i += 1;
            continue;
          }
          b'"' => {
            state = State::DoubleString;
            i += 1;
            continue;
          }
          b'`' => {
            state = State::TemplateString;
            i += 1;
            continue;
          }
          _ => {}
        }

        if i + token.len() <= end && &bytes[i..i + token.len()] == token {
          let prev = if i == start { None } else { Some(bytes[i - 1]) };
          let next = bytes.get(i + token.len()).copied();
          let prev_ok = prev.map(|b| !is_ident_char(b)).unwrap_or(true);
          let next_ok = next.map(|b| !is_ident_char(b)).unwrap_or(true);
          if prev_ok && next_ok {
            found = Some(TextRange::new(i as u32, (i + token.len()) as u32));
          }
        }

        i += 1;
      }
      State::LineComment => {
        if bytes[i] == b'\n' {
          state = State::Code;
        }
        i += 1;
      }
      State::BlockComment => {
        if bytes[i] == b'*' && i + 1 < end && bytes[i + 1] == b'/' {
          state = State::Code;
          i += 2;
        } else {
          i += 1;
        }
      }
      State::SingleString => {
        if bytes[i] == b'\\' && i + 1 < end {
          i += 2;
        } else if bytes[i] == b'\'' {
          state = State::Code;
          i += 1;
        } else {
          i += 1;
        }
      }
      State::DoubleString => {
        if bytes[i] == b'\\' && i + 1 < end {
          i += 2;
        } else if bytes[i] == b'"' {
          state = State::Code;
          i += 1;
        } else {
          i += 1;
        }
      }
      State::TemplateString => {
        if bytes[i] == b'\\' && i + 1 < end {
          i += 2;
        } else if bytes[i] == b'`' {
          state = State::Code;
          i += 1;
        } else {
          i += 1;
        }
      }
    }
  }

  found
}
