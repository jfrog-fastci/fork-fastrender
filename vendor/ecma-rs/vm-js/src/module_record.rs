use crate::execution_context::ModuleId;
use crate::module_graph::ModuleGraph;
use crate::ImportAttribute;
use crate::LoadedModuleRequest;
use crate::ModuleRequest;
use crate::RootId;
use crate::Vm;
use crate::VmError;
use diagnostics::{Diagnostic, FileId};
use parse_js::ast::class_or_object::{
  ClassMember, ClassOrObjKey, ClassOrObjVal, ObjMember, ObjMemberType,
};
use parse_js::ast::expr::Expr;
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::lit::{LitArrElem, LitTemplatePart};
use parse_js::ast::import_export::ExportNames;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stmt::ForInOfLhs;
use parse_js::ast::stx::TopLevel;
use parse_js::lex::KEYWORDS_MAPPING;
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::collections::HashSet;

/// Module linking/loading status.
///
/// This is a minimal subset of ECMA-262's `ModuleStatus` enum; additional states will be added as
/// module linking/evaluation are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModuleStatus {
  #[default]
  New,
  Unlinked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalExportEntry {
  pub export_name: String,
  pub local_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportName {
  Name(String),
  /// Corresponds to ECMA-262 `ImportName = all`, used by `export * as ns from "m"`.
  All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndirectExportEntry {
  pub export_name: String,
  pub module_request: ModuleRequest,
  pub import_name: ImportName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StarExportEntry {
  pub module_request: ModuleRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingName {
  Name(String),
  Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBinding {
  pub module: ModuleId,
  pub binding_name: BindingName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveExportResult {
  Resolved(ResolvedBinding),
  NotFound,
  Ambiguous,
}

/// Cached data for a module's namespace object (`module.[[Namespace]]` in ECMA-262).
#[derive(Clone, Debug)]
pub(crate) struct ModuleNamespaceCache {
  pub object: RootId,
  pub exports: Vec<String>,
}

/// Source Text Module Record (ECMA-262).
#[derive(Clone, Debug, Default)]
pub struct SourceTextModuleRecord {
  pub requested_modules: Vec<ModuleRequest>,
  pub status: ModuleStatus,
  /// `[[HasTLA]]` – whether this module contains top-level `await`.
  pub has_tla: bool,
  pub local_export_entries: Vec<LocalExportEntry>,
  pub indirect_export_entries: Vec<IndirectExportEntry>,
  pub star_export_entries: Vec<StarExportEntry>,

  /// `[[LoadedModules]]` – a host-populated mapping from module requests to resolved module ids.
  pub loaded_modules: Vec<LoadedModuleRequest<ModuleId>>,

  /// `[[Namespace]]` – cached module namespace object + sorted `[[Exports]]` list.
  ///
  /// Note: the namespace object is rooted in the heap via a persistent [`RootId`] so it survives GC.
  pub(crate) namespace: Option<ModuleNamespaceCache>,
}

impl SourceTextModuleRecord {
  /// Returns the cached namespace export list (`[[Exports]]`) if a namespace object has been
  /// created.
  pub fn namespace_exports(&self) -> Option<&[String]> {
    self.namespace.as_ref().map(|ns| ns.exports.as_slice())
  }

  /// Parses a source text module using the `parse-js` front-end and extracts the module record
  /// fields needed by `GetExportedNames` and `ResolveExport`.
  ///
  /// This corresponds to the spec's `ParseModule` abstract operation, but only models the export
  /// entry lists and `[[RequestedModules]]`.
  pub fn parse(source: &str) -> Result<Self, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = parse_with_options(source, opts)
      .map_err(|err| VmError::Syntax(vec![err.to_diagnostic(FileId(0))]))?;

    let mut cancel = || Ok(());
    module_record_from_top_level(&top, &mut cancel)
  }

  /// Parses a source text module using VM budget/interrupt state.
  pub fn parse_with_vm(vm: &mut Vm, source: &str) -> Result<Self, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = vm.parse_top_level_with_budget(source, opts)?;

    let mut cancel = || vm.tick();
    module_record_from_top_level(&top, &mut cancel)
  }

  /// Implements ECMA-262 `GetExportedNames([exportStarSet])`.
  pub fn get_exported_names(&self, graph: &ModuleGraph, module: ModuleId) -> Vec<String> {
    self.get_exported_names_with_star_set(graph, module, &mut Vec::new())
  }

  pub fn get_exported_names_with_star_set(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_star_set: &mut Vec<ModuleId>,
  ) -> Vec<String> {
    // 1. If exportStarSet contains module, then
    if export_star_set.contains(&module) {
      // a. Return a new empty List.
      return Vec::new();
    }

    // 2. Append module to exportStarSet.
    export_star_set.push(module);

    // 3. Let exportedNames be a new empty List.
    let mut exported_names = Vec::<String>::new();

    // 4. For each ExportEntry Record e of module.[[LocalExportEntries]], do
    for entry in &self.local_export_entries {
      // a. Append e.[[ExportName]] to exportedNames.
      exported_names.push(entry.export_name.clone());
    }

    // 5. For each ExportEntry Record e of module.[[IndirectExportEntries]], do
    for entry in &self.indirect_export_entries {
      // a. Append e.[[ExportName]] to exportedNames.
      exported_names.push(entry.export_name.clone());
    }

    // 6. For each ExportEntry Record e of module.[[StarExportEntries]], do
    for entry in &self.star_export_entries {
      // a. Let requestedModule be GetImportedModule(module, e.[[ModuleRequest]]).
      let Some(requested_module) = graph.get_imported_module(module, &entry.module_request) else {
        continue;
      };
      // b. Let starNames be requestedModule.GetExportedNames(exportStarSet).
      let star_names =
        graph
          .module(requested_module)
          .get_exported_names_with_star_set(graph, requested_module, export_star_set);

      // c. For each element n of starNames, do
      for name in star_names {
        // i. If SameValue(n, "default") is false, then
        if name == "default" {
          continue;
        }
        // 1. If exportedNames does not contain n, then
        if !exported_names.contains(&name) {
          // a. Append n to exportedNames.
          exported_names.push(name);
        }
      }
    }

    // 7. Return exportedNames.
    exported_names
  }

  /// Implements ECMA-262 `ResolveExport(exportName[, resolveSet])`.
  pub fn resolve_export(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
  ) -> ResolveExportResult {
    self.resolve_export_with_set(graph, module, export_name, &mut Vec::new())
  }

  pub fn resolve_export_with_set(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
    resolve_set: &mut Vec<(ModuleId, String)>,
  ) -> ResolveExportResult {
    // 1. For each Record { [[Module]], [[ExportName]] } r of resolveSet, do
    //    a. If module and r.[[Module]] are the same Module Record and SameValue(exportName, r.[[ExportName]]) is true, then
    //       i. Return null.
    if resolve_set
      .iter()
      .any(|(m, name)| *m == module && name == export_name)
    {
      return ResolveExportResult::NotFound;
    }

    // 2. Append the Record { [[Module]]: module, [[ExportName]]: exportName } to resolveSet.
    resolve_set.push((module, export_name.to_string()));

    // 3. For each ExportEntry Record e of module.[[LocalExportEntries]], do
    for entry in &self.local_export_entries {
      // a. If SameValue(exportName, e.[[ExportName]]) is true, then
      if entry.export_name == export_name {
        // i. Assert: module provides the direct binding for this export.
        // ii. Return ResolvedBinding Record { [[Module]]: module, [[BindingName]]: e.[[LocalName]] }.
        return ResolveExportResult::Resolved(ResolvedBinding {
          module,
          binding_name: BindingName::Name(entry.local_name.clone()),
        });
      }
    }

    // 4. For each ExportEntry Record e of module.[[IndirectExportEntries]], do
    for entry in &self.indirect_export_entries {
      // a. If SameValue(exportName, e.[[ExportName]]) is true, then
      if entry.export_name == export_name {
        // i. Let importedModule be GetImportedModule(module, e.[[ModuleRequest]]).
        let Some(imported_module) = graph.get_imported_module(module, &entry.module_request) else {
          return ResolveExportResult::NotFound;
        };
        // ii. If e.[[ImportName]] is all, then
        if entry.import_name == ImportName::All {
          // 1. Return ResolvedBinding Record { [[Module]]: importedModule, [[BindingName]]: namespace }.
          return ResolveExportResult::Resolved(ResolvedBinding {
            module: imported_module,
            binding_name: BindingName::Namespace,
          });
        }

        // iii. Else,
        // 1. Assert: e.[[ImportName]] is a String.
        // 2. Return importedModule.ResolveExport(e.[[ImportName]], resolveSet).
        let import_name = match &entry.import_name {
          ImportName::Name(name) => name,
          ImportName::All => {
            debug_assert!(false, "ImportName::All handled above");
            return ResolveExportResult::NotFound;
          }
        };
        return graph
          .module(imported_module)
          .resolve_export_with_set(graph, imported_module, import_name, resolve_set);
      }
    }

    // 5. If SameValue(exportName, "default") is true, then
    if export_name == "default" {
      // a. Return null.
      return ResolveExportResult::NotFound;
    }

    // 6. Let starResolution be null.
    let mut star_resolution: Option<ResolvedBinding> = None;

    // 7. For each ExportEntry Record e of module.[[StarExportEntries]], do
    for entry in &self.star_export_entries {
      // a. Let importedModule be GetImportedModule(module, e.[[ModuleRequest]]).
      let Some(imported_module) = graph.get_imported_module(module, &entry.module_request) else {
        continue;
      };
      // b. Let resolution be importedModule.ResolveExport(exportName, resolveSet).
      let resolution = graph
        .module(imported_module)
        .resolve_export_with_set(graph, imported_module, export_name, resolve_set);

      // c. If resolution is ambiguous, return ambiguous.
      if resolution == ResolveExportResult::Ambiguous {
        return ResolveExportResult::Ambiguous;
      }

      // d. If resolution is not null, then
      let ResolveExportResult::Resolved(resolution) = resolution else {
        continue;
      };

      // i. If starResolution is null, then
      let Some(existing) = &star_resolution else {
        // 1. Set starResolution to resolution.
        star_resolution = Some(resolution);
        continue;
      };

      // ii. Else,
      // 1. If resolution.[[Module]] and starResolution.[[Module]] are not the same Module Record, return ambiguous.
      // 2. If resolution.[[BindingName]] is not the same as starResolution.[[BindingName]], return ambiguous.
      if existing != &resolution {
        return ResolveExportResult::Ambiguous;
      }
    }

    // 8. Return starResolution.
    match star_resolution {
      Some(binding) => ResolveExportResult::Resolved(binding),
      None => ResolveExportResult::NotFound,
    }
  }
}

const MODULE_RECORD_TICK_EVERY: u64 = 256;

struct ModuleRecordParseCtx<'a> {
  steps: u64,
  cancel: &'a mut dyn FnMut() -> Result<(), VmError>,
}

impl<'a> ModuleRecordParseCtx<'a> {
  fn new(cancel: &'a mut dyn FnMut() -> Result<(), VmError>) -> Self {
    Self { steps: 0, cancel }
  }

  fn cancel_now(&mut self) -> Result<(), VmError> {
    (self.cancel)()
  }

  fn budget_tick(&mut self) -> Result<(), VmError> {
    self.steps = self.steps.wrapping_add(1);
    if self.steps % MODULE_RECORD_TICK_EVERY == 0 {
      (self.cancel)()?;
    }
    Ok(())
  }
}

fn module_record_from_top_level(
  top: &Node<TopLevel>,
  cancel: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<SourceTextModuleRecord, VmError> {
  let mut ctx = ModuleRecordParseCtx::new(cancel);
  ctx.cancel_now()?;

  let mut record = SourceTextModuleRecord::default();
  record.has_tla = module_contains_top_level_await(top, &mut ctx)?;

  for stmt in &top.stx.body {
    ctx.budget_tick()?;

    match &*stmt.stx {
      Stmt::Import(import_stmt) => {
        if import_stmt.stx.type_only {
          continue;
        }
        let req = module_request_from_specifier(
          &import_stmt.stx.module,
          import_stmt.stx.attributes.as_ref(),
          &mut ctx,
        )?;
        push_requested_module(&mut record.requested_modules, req, &mut ctx)?;
      }

      Stmt::ExportDefaultExpr(_) => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        record.local_export_entries.push(LocalExportEntry {
          export_name: try_string_from_str("default")?,
          local_name: try_string_from_str("*default*")?,
        });
      }

      Stmt::ExportList(export_stmt) => {
        if export_stmt.stx.type_only {
          continue;
        }

        let from = match export_stmt.stx.from.as_ref() {
          Some(specifier) => Some(module_request_from_specifier(
            specifier,
            export_stmt.stx.attributes.as_ref(),
            &mut ctx,
          )?),
          None => None,
        };

        if let Some(req) = &from {
          let mut exists = false;
          for existing in &record.requested_modules {
            ctx.budget_tick()?;
            if existing == req {
              exists = true;
              break;
            }
          }
          if !exists {
            record
              .requested_modules
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record
              .requested_modules
              .push(clone_module_request(req, &mut ctx)?);
          }
        }

        match (&export_stmt.stx.names, from) {
          (ExportNames::All(None), Some(req)) => {
            record
              .star_export_entries
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record.star_export_entries.push(StarExportEntry { module_request: req });
          }
          (ExportNames::All(Some(alias)), Some(req)) => {
            record
              .indirect_export_entries
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record.indirect_export_entries.push(IndirectExportEntry {
              export_name: try_string_from_str(&alias.stx.name)?,
              module_request: req,
              import_name: ImportName::All,
            });
          }
          (ExportNames::Specific(names), Some(req)) => {
            record
              .indirect_export_entries
              .try_reserve(names.len())
              .map_err(|_| VmError::OutOfMemory)?;
            if let Some((last, rest)) = names.split_last() {
              for name in rest {
                ctx.budget_tick()?;
                record.indirect_export_entries.push(IndirectExportEntry {
                  export_name: try_string_from_str(&name.stx.alias.stx.name)?,
                  module_request: clone_module_request(&req, &mut ctx)?,
                  import_name: ImportName::Name(try_string_from_str(name.stx.exportable.as_str())?),
                });
              }
              ctx.budget_tick()?;
              record.indirect_export_entries.push(IndirectExportEntry {
                export_name: try_string_from_str(&last.stx.alias.stx.name)?,
                module_request: req,
                import_name: ImportName::Name(try_string_from_str(last.stx.exportable.as_str())?),
              });
            }
          }
          (ExportNames::Specific(names), None) => {
            record
              .local_export_entries
              .try_reserve(names.len())
              .map_err(|_| VmError::OutOfMemory)?;
            for name in names {
              ctx.budget_tick()?;
              record.local_export_entries.push(LocalExportEntry {
                export_name: try_string_from_str(&name.stx.alias.stx.name)?,
                local_name: try_string_from_str(name.stx.exportable.as_str())?,
              });
            }
          }
          (ExportNames::All(_), None) => {}
        }
      }

      Stmt::VarDecl(var_decl) if var_decl.stx.export => {
        record
          .local_export_entries
          .try_reserve(var_decl.stx.declarators.len())
          .map_err(|_| VmError::OutOfMemory)?;

        for declarator in &var_decl.stx.declarators {
          ctx.budget_tick()?;

          let pat = &declarator.pattern.stx.pat;
          let local_name = match &*pat.stx {
            Pat::Id(id) => try_string_from_str(&id.stx.name)?,
            _ => return Err(VmError::Unimplemented("exported destructuring patterns")),
          };

          record.local_export_entries.push(LocalExportEntry {
            export_name: try_string_from_str(&local_name)?,
            local_name,
          });
        }
      }

      Stmt::FunctionDecl(func) if func.stx.export || func.stx.export_default => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        let local_name = match func.stx.name.as_ref() {
          Some(n) => try_string_from_str(&n.stx.name)?,
          None => try_string_from_str("*default*")?,
        };
        record.local_export_entries.push(LocalExportEntry {
          export_name: if func.stx.export {
            try_string_from_str(&local_name)?
          } else {
            try_string_from_str("default")?
          },
          local_name,
        });
      }

      Stmt::ClassDecl(class) if class.stx.export || class.stx.export_default => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        let local_name = match class.stx.name.as_ref() {
          Some(n) => try_string_from_str(&n.stx.name)?,
          None => try_string_from_str("*default*")?,
        };
        record.local_export_entries.push(LocalExportEntry {
          export_name: if class.stx.export {
            try_string_from_str(&local_name)?
          } else {
            try_string_from_str("default")?
          },
          local_name,
        });
      }

      _ => {}
    }
  }

  Ok(record)
}

fn module_contains_top_level_await(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  stmt_list_contains_top_level_await(&top.stx.body, ctx)
}

fn stmt_list_contains_top_level_await(
  stmts: &[Node<Stmt>],
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  for stmt in stmts {
    if stmt_contains_top_level_await(stmt, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn stmt_contains_top_level_await(
  stmt: &Node<Stmt>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  Ok(match &*stmt.stx {
    Stmt::Expr(expr_stmt) => expr_contains_top_level_await(&expr_stmt.stx.expr, ctx)?,
    Stmt::Block(block) => stmt_list_contains_top_level_await(&block.stx.body, ctx)?,
    Stmt::DoWhile(stmt) => {
      expr_contains_top_level_await(&stmt.stx.condition, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }
    Stmt::If(stmt) => {
      if expr_contains_top_level_await(&stmt.stx.test, ctx)? {
        true
      } else if stmt_contains_top_level_await(&stmt.stx.consequent, ctx)? {
        true
      } else if let Some(alt) = stmt.stx.alternate.as_ref() {
        stmt_contains_top_level_await(alt, ctx)?
      } else {
        false
      }
    }
    Stmt::While(stmt) => {
      expr_contains_top_level_await(&stmt.stx.condition, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }
    Stmt::ForTriple(stmt) => {
      let init_has = match &stmt.stx.init {
        parse_js::ast::stmt::ForTripleStmtInit::None => false,
        parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => expr_contains_top_level_await(expr, ctx)?,
        parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
          var_decl_contains_top_level_await(&decl.stx, ctx)?
        }
      };
      let cond_has = match stmt.stx.cond.as_ref() {
        Some(expr) => expr_contains_top_level_await(expr, ctx)?,
        None => false,
      };
      let post_has = match stmt.stx.post.as_ref() {
        Some(expr) => expr_contains_top_level_await(expr, ctx)?,
        None => false,
      };
      init_has || cond_has || post_has || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
    }
    Stmt::ForIn(stmt) => {
      for_in_of_lhs_contains_top_level_await(&stmt.stx.lhs, ctx)?
        || expr_contains_top_level_await(&stmt.stx.rhs, ctx)?
        || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
    }
    Stmt::ForOf(stmt) => {
      if stmt.stx.await_ {
        true
      } else {
        for_in_of_lhs_contains_top_level_await(&stmt.stx.lhs, ctx)?
          || expr_contains_top_level_await(&stmt.stx.rhs, ctx)?
          || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
      }
    }
    Stmt::Label(stmt) => stmt_contains_top_level_await(&stmt.stx.statement, ctx)?,
    Stmt::Switch(stmt) => {
      if expr_contains_top_level_await(&stmt.stx.test, ctx)? {
        true
      } else {
        let mut found = false;
        for branch in &stmt.stx.branches {
          if let Some(case) = branch.stx.case.as_ref() {
            if expr_contains_top_level_await(case, ctx)? {
              found = true;
              break;
            }
          }
          if stmt_list_contains_top_level_await(&branch.stx.body, ctx)? {
            found = true;
            break;
          }
        }
        found
      }
    }
    Stmt::Throw(stmt) => expr_contains_top_level_await(&stmt.stx.value, ctx)?,
    Stmt::Try(stmt) => {
      if stmt_list_contains_top_level_await(&stmt.stx.wrapped.stx.body, ctx)? {
        true
      } else {
        if let Some(catch) = stmt.stx.catch.as_ref() {
          if stmt_list_contains_top_level_await(&catch.stx.body, ctx)? {
            return Ok(true);
          }
        }
        if let Some(finally) = stmt.stx.finally.as_ref() {
          if stmt_list_contains_top_level_await(&finally.stx.body, ctx)? {
            return Ok(true);
          }
        }
        false
      }
    }
    Stmt::With(stmt) => {
      expr_contains_top_level_await(&stmt.stx.object, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }

    // Import/export statements.
    Stmt::ExportDefaultExpr(stmt) => expr_contains_top_level_await(&stmt.stx.expression, ctx)?,
    Stmt::ExportList(stmt) => match stmt.stx.attributes.as_ref() {
      Some(attributes) => expr_contains_top_level_await(attributes, ctx)?,
      None => false,
    },
    Stmt::Import(stmt) => match stmt.stx.attributes.as_ref() {
      Some(attributes) => expr_contains_top_level_await(attributes, ctx)?,
      None => false,
    },

    // Declarations.
    Stmt::ClassDecl(stmt) => class_decl_contains_top_level_await(&stmt.stx, ctx)?,
    Stmt::VarDecl(stmt) => var_decl_contains_top_level_await(&stmt.stx, ctx)?,

    // Function-like boundaries: do not descend.
    Stmt::FunctionDecl(_) => false,

    // Everything else cannot contain `await` (or is syntax-error in modules).
    _ => false,
  })
}

fn var_decl_contains_top_level_await(
  decl: &parse_js::ast::stmt::decl::VarDecl,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &decl.declarators {
    if pat_contains_top_level_await(&d.pattern.stx.pat, ctx)? {
      return Ok(true);
    }
    if let Some(init) = d.initializer.as_ref() {
      if expr_contains_top_level_await(init, ctx)? {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

fn for_in_of_lhs_contains_top_level_await(
  lhs: &ForInOfLhs,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match lhs {
    ForInOfLhs::Assign(pat) => pat_contains_top_level_await(pat, ctx),
    ForInOfLhs::Decl((_mode, pat_decl)) => pat_contains_top_level_await(&pat_decl.stx.pat, ctx),
  }
}

fn expr_contains_top_level_await(
  expr: &Node<Expr>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  Ok(match &*expr.stx {
    Expr::Unary(unary) => {
      if unary.stx.operator == OperatorName::Await {
        true
      } else {
        expr_contains_top_level_await(&unary.stx.argument, ctx)?
      }
    }
    Expr::UnaryPostfix(unary) => expr_contains_top_level_await(&unary.stx.argument, ctx)?,
    Expr::Binary(binary) => {
      expr_contains_top_level_await(&binary.stx.left, ctx)?
        || expr_contains_top_level_await(&binary.stx.right, ctx)?
    }
    Expr::Call(call) => {
      if expr_contains_top_level_await(&call.stx.callee, ctx)? {
        true
      } else {
        let mut found = false;
        for arg in &call.stx.arguments {
          if expr_contains_top_level_await(&arg.stx.value, ctx)? {
            found = true;
            break;
          }
        }
        found
      }
    }
    Expr::ComputedMember(member) => {
      expr_contains_top_level_await(&member.stx.object, ctx)?
        || expr_contains_top_level_await(&member.stx.member, ctx)?
    }
    Expr::Cond(cond) => {
      expr_contains_top_level_await(&cond.stx.test, ctx)?
        || expr_contains_top_level_await(&cond.stx.consequent, ctx)?
        || expr_contains_top_level_await(&cond.stx.alternate, ctx)?
    }
    Expr::Import(expr) => {
      if expr_contains_top_level_await(&expr.stx.module, ctx)? {
        true
      } else if let Some(attrs) = expr.stx.attributes.as_ref() {
        expr_contains_top_level_await(attrs, ctx)?
      } else {
        false
      }
    }
    Expr::Member(member) => expr_contains_top_level_await(&member.stx.left, ctx)?,
    Expr::TaggedTemplate(template) => {
      if expr_contains_top_level_await(&template.stx.function, ctx)? {
        true
      } else {
        let mut found = false;
        for part in &template.stx.parts {
          if let LitTemplatePart::Substitution(expr) = part {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
        }
        found
      }
    }

    Expr::LitArr(arr) => {
      let mut found = false;
      for elem in &arr.stx.elements {
        match elem {
          LitArrElem::Single(expr) | LitArrElem::Rest(expr) => {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
          LitArrElem::Empty => {
            // Array literals can contain arbitrarily many elisions (`[,,,,]`) without any nested
            // expressions. Budget traversal so module record parsing can't do `O(N)` work without
            // calling the cancel/budget hook.
            ctx.budget_tick()?;
          }
        }
      }
      found
    }
    Expr::LitObj(obj) => {
      let mut found = false;
      for member in &obj.stx.members {
        if obj_member_contains_top_level_await(member, ctx)? {
          found = true;
          break;
        }
      }
      found
    }
    Expr::LitTemplate(template) => {
      let mut found = false;
      for part in &template.stx.parts {
        match part {
          LitTemplatePart::Substitution(expr) => {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
          LitTemplatePart::String(_) => {}
        }
      }
      found
    }

    // Class expressions are not function boundaries: only method bodies are.
    Expr::Class(class) => class_expr_contains_top_level_await(&class.stx, ctx)?,

    // Patterns (can contain expressions via default values).
    Expr::ArrPat(arr) => arr_pat_contains_top_level_await(&arr.stx, ctx)?,
    Expr::IdPat(_) => false,
    Expr::ObjPat(obj) => obj_pat_contains_top_level_await(&obj.stx, ctx)?,

    // TypeScript wrappers around expressions.
    Expr::Instantiation(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::TypeAssertion(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::NonNullAssertion(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::SatisfiesExpr(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,

    // Function-like boundaries: do not descend.
    Expr::ArrowFunc(_) | Expr::Func(_) => false,

    // Everything else is leaf-like for our purposes.
    _ => false,
  })
}

fn pat_contains_top_level_await(
  pat: &Node<Pat>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match &*pat.stx {
    Pat::Arr(arr) => arr_pat_contains_top_level_await(&arr.stx, ctx),
    Pat::Obj(obj) => obj_pat_contains_top_level_await(&obj.stx, ctx),
    Pat::AssignTarget(expr) => expr_contains_top_level_await(expr, ctx),
    Pat::Id(_) => Ok(false),
  }
}

fn arr_pat_contains_top_level_await(
  pat: &parse_js::ast::expr::pat::ArrPat,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for elem in &pat.elements {
    let Some(elem) = elem.as_ref() else {
      // Array patterns can contain arbitrarily many elisions (`[,,,,x]`) without any nested
      // patterns/expressions. Budget traversal so top-level-await scanning can't do `O(N)` work
      // without calling the cancel/budget hook.
      ctx.budget_tick()?;
      continue;
    };
    if pat_contains_top_level_await(&elem.target, ctx)? {
      return Ok(true);
    }
    if let Some(default) = elem.default_value.as_ref() {
      if expr_contains_top_level_await(default, ctx)? {
        return Ok(true);
      }
    }
  }

  if let Some(rest) = pat.rest.as_ref() {
    return pat_contains_top_level_await(rest, ctx);
  }
  Ok(false)
}

fn obj_pat_contains_top_level_await(
  pat: &parse_js::ast::expr::pat::ObjPat,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for prop in &pat.properties {
    if class_or_obj_key_contains_top_level_await(&prop.stx.key, ctx)? {
      return Ok(true);
    }
    if pat_contains_top_level_await(&prop.stx.target, ctx)? {
      return Ok(true);
    }
    if let Some(default) = prop.stx.default_value.as_ref() {
      if expr_contains_top_level_await(default, ctx)? {
        return Ok(true);
      }
    }
  }

  if let Some(rest) = pat.rest.as_ref() {
    return pat_contains_top_level_await(rest, ctx);
  }
  Ok(false)
}

fn class_decl_contains_top_level_await(
  class: &parse_js::ast::stmt::decl::ClassDecl,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &class.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  if let Some(extends) = class.extends.as_ref() {
    if expr_contains_top_level_await(extends, ctx)? {
      return Ok(true);
    }
  }
  for imp in &class.implements {
    if expr_contains_top_level_await(imp, ctx)? {
      return Ok(true);
    }
  }
  for member in &class.members {
    if class_member_contains_top_level_await(member, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn class_expr_contains_top_level_await(
  class: &parse_js::ast::expr::ClassExpr,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &class.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  if let Some(extends) = class.extends.as_ref() {
    if expr_contains_top_level_await(extends, ctx)? {
      return Ok(true);
    }
  }
  for member in &class.members {
    if class_member_contains_top_level_await(member, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn class_member_contains_top_level_await(
  member: &Node<ClassMember>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &member.stx.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  Ok(
    class_or_obj_key_contains_top_level_await(&member.stx.key, ctx)?
      || class_or_obj_val_contains_top_level_await(&member.stx.val, ctx)?,
  )
}

fn obj_member_contains_top_level_await(
  member: &Node<ObjMember>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match &member.stx.typ {
    ObjMemberType::Valued { key, val } => Ok(
      class_or_obj_key_contains_top_level_await(key, ctx)?
        || class_or_obj_val_contains_top_level_await(val, ctx)?,
    ),
    ObjMemberType::Shorthand { .. } => Ok(false),
    ObjMemberType::Rest { val } => expr_contains_top_level_await(val, ctx),
  }
}

fn class_or_obj_key_contains_top_level_await(
  key: &ClassOrObjKey,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match key {
    ClassOrObjKey::Direct(_) => Ok(false),
    ClassOrObjKey::Computed(expr) => expr_contains_top_level_await(expr, ctx),
  }
}

fn class_or_obj_val_contains_top_level_await(
  val: &ClassOrObjVal,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match val {
    // Function-like boundaries: do not descend.
    ClassOrObjVal::Getter(_) | ClassOrObjVal::Setter(_) | ClassOrObjVal::Method(_) => Ok(false),
    ClassOrObjVal::Prop(expr) => match expr.as_ref() {
      Some(expr) => expr_contains_top_level_await(expr, ctx),
      None => Ok(false),
    },
    ClassOrObjVal::IndexSignature(_) => Ok(false),
    // Class static blocks are syntax-errors for `await`; don't scan them.
    ClassOrObjVal::StaticBlock(_) => Ok(false),
  }
}

fn module_request_from_specifier(
  specifier: &str,
  attributes: Option<&Node<Expr>>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<ModuleRequest, VmError> {
  ctx.budget_tick()?;
  Ok(ModuleRequest::new(
    try_string_from_str(specifier)?,
    with_clause_to_attributes(attributes, ctx)?,
  ))
}

fn push_requested_module(
  out: &mut Vec<ModuleRequest>,
  request: ModuleRequest,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  for existing in out.iter() {
    ctx.budget_tick()?;
    // All `ModuleRequest`s we create here are canonicalized via `ModuleRequest::new` (attribute list
    // sorting), so a direct equality check is equivalent to `ModuleRequestsEqual` while being much
    // cheaper than the spec-shaped order-insensitive comparison.
    if existing == &request {
      return Ok(());
    }
  }
  out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  out.push(request);
  Ok(())
}

/// Implements `WithClauseToAttributes` (ECMA-262) for static import/export declarations.
fn with_clause_to_attributes(
  attributes: Option<&Node<Expr>>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<Vec<ImportAttribute>, VmError> {
  let Some(attributes) = attributes else {
    return Ok(Vec::new());
  };

  ctx.budget_tick()?;

  let Expr::LitObj(obj) = &*attributes.stx else {
    return Err(syntax_error(
      attributes.loc,
      "import attributes must be an object literal",
    ));
  };

  let mut seen = HashSet::<&str>::new();
  let mut out = Vec::<ImportAttribute>::new();

  seen
    .try_reserve(obj.stx.members.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out
    .try_reserve(obj.stx.members.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for member in &obj.stx.members {
    ctx.budget_tick()?;

    let (key_str, key_loc, value_expr) = match &member.stx.typ {
      ObjMemberType::Valued { key, val } => {
        let key_node = match key {
          ClassOrObjKey::Direct(direct) => direct,
          ClassOrObjKey::Computed(_) => {
            return Err(syntax_error(
              member.loc,
              "computed import attribute keys are not allowed",
            ));
          }
        };

        let is_ident_or_keyword =
          key_node.stx.tt == TT::Identifier || KEYWORDS_MAPPING.contains_key(&key_node.stx.tt);
        let is_string = key_node.stx.tt == TT::LiteralString;
        if !is_ident_or_keyword && !is_string {
          return Err(syntax_error(
            key_node.loc,
            "import attribute keys must be identifiers, keywords, or string literals",
          ));
        }

        let value_expr = match val {
          ClassOrObjVal::Prop(Some(expr)) => expr,
          _ => {
            return Err(syntax_error(
              member.loc,
              "import attribute entries must be simple key/value properties",
            ));
          }
        };

        (key_node.stx.key.as_str(), key_node.loc, value_expr)
      }
      ObjMemberType::Shorthand { .. } => {
        return Err(syntax_error(
          member.loc,
          "shorthand properties are not allowed in import attributes",
        ));
      }
      ObjMemberType::Rest { .. } => {
        return Err(syntax_error(
          member.loc,
          "spread properties are not allowed in import attributes",
        ));
      }
    };

    if !seen.insert(key_str) {
      return Err(syntax_error(key_loc, "duplicate import attribute key"));
    }

    let key = try_string_from_str(key_str)?;
    let value = match &*value_expr.stx {
      Expr::LitStr(str_lit) => try_string_from_str(&str_lit.stx.value)?,
      _ => {
        return Err(syntax_error(
          value_expr.loc,
          "import attribute values must be string literals",
        ));
      }
    };

    out.push(ImportAttribute { key, value });
  }

  // `ModuleRequest::new` canonicalizes attribute order for storage/comparison, so callers that turn
  // this into a `ModuleRequest` do not need to pre-sort here.
  Ok(out)
}

fn try_string_from_str(value: &str) -> Result<String, VmError> {
  let mut out = String::new();
  out.try_reserve(value.len()).map_err(|_| VmError::OutOfMemory)?;
  out.push_str(value);
  Ok(out)
}

fn clone_module_request(
  req: &ModuleRequest,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<ModuleRequest, VmError> {
  ctx.budget_tick()?;
  let mut attrs = Vec::<ImportAttribute>::new();
  attrs
    .try_reserve(req.attributes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for attr in &req.attributes {
    ctx.budget_tick()?;
    attrs.push(ImportAttribute {
      key: try_string_from_str(&attr.key)?,
      value: try_string_from_str(&attr.value)?,
    });
  }

  Ok(ModuleRequest::new(
    try_string_from_str(&req.specifier)?,
    attrs,
  ))
}

fn syntax_error(loc: parse_js::loc::Loc, message: &str) -> VmError {
  let span = loc.to_diagnostics_span(FileId(0));
  VmError::Syntax(vec![Diagnostic::error("VMJS0001", message, span)])
}
