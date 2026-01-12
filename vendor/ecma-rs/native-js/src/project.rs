use crate::codegen::llvm::{LlvmModuleBuilder, Ty, UserFunctionSig};
use crate::codegen::CodegenError;
use crate::{CompileOptions, NativeJsError};
use hir_js::ImportKind;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::type_expr::TypeExpr;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use typecheck_ts::{DefId, DefKind, FileId, FileKey, Host, ImportTarget, Program, TextRange};

#[derive(Debug, Default, Clone)]
struct ModuleInfo {
  /// Runtime dependencies (value imports + side-effect imports), in source order.
  deps: Vec<FileId>,
  /// Import bindings in this file (`import { foo as bar } from "..."`).
  import_bindings: Vec<ImportBinding>,
}

#[derive(Debug, Clone)]
struct ImportBinding {
  dep: FileId,
  export_name: String,
  local_name: String,
}

fn file_label(program: &Program, file: FileId) -> String {
  program
    .file_key(file)
    .map(|k| k.to_string())
    .unwrap_or_else(|| format!("file{}", file.0))
}

fn file_key_or_fallback(program: &Program, file: FileId) -> FileKey {
  program
    .file_key(file)
    .unwrap_or_else(|| FileKey::new(format!("file{}.ts", file.0)))
}

fn dialect_for_kind(kind: typecheck_ts::lib_support::FileKind) -> Dialect {
  use typecheck_ts::lib_support::FileKind as K;
  match kind {
    K::Ts => Dialect::Ts,
    K::Tsx => Dialect::Tsx,
    K::Dts => Dialect::Dts,
    K::Js => Dialect::Js,
    K::Jsx => Dialect::Jsx,
  }
}

fn ty_from_type_expr(expr: &Node<TypeExpr>) -> Result<Ty, NativeJsError> {
  match expr.stx.as_ref() {
    TypeExpr::Number(_) => Ok(Ty::Number),
    TypeExpr::Boolean(_) => Ok(Ty::Bool),
    TypeExpr::String(_) => Ok(Ty::String),
    TypeExpr::Void(_) => Ok(Ty::Void),
    TypeExpr::Null(_) => Ok(Ty::Null),
    TypeExpr::Undefined(_) => Ok(Ty::Undefined),
    other => Err(NativeJsError::Codegen(CodegenError::TypeError {
      message: format!("unsupported type annotation: {other:?}"),
      loc: expr.loc,
    })),
  }
}

fn ty_from_opt_type_expr(expr: Option<&Node<TypeExpr>>) -> Result<Ty, NativeJsError> {
  match expr {
    Some(expr) => ty_from_type_expr(expr),
    None => Ok(Ty::Number),
  }
}

fn is_runtime_import(es: &hir_js::ImportEs) -> bool {
  if es.is_type_only {
    return false;
  }
  if es.default.is_some() || es.namespace.is_some() {
    return true;
  }
  if es.named.iter().any(|s| !s.is_type_only) {
    return true;
  }
  // `import "foo";` or `import {} from "foo";`
  es.named.is_empty()
}

fn resolve_import_def(program: &Program, file: FileId, span: TextRange) -> Option<DefId> {
  let sym = program.symbol_at(file, span.start)?;
  program.symbol_info(sym)?.def
}

fn resolve_export_def(program: &Program, file: FileId, export_name: &str) -> Option<DefId> {
  let (symbol, local_def) = {
    let exports = program.exports_of(file);
    let entry = exports.get(export_name)?;
    (entry.symbol, entry.def)
  };
  let mut cur = program
    .symbol_info(symbol)
    .and_then(|info| info.def)
    .or(local_def)?;

  let mut seen = BTreeSet::<DefId>::new();
  loop {
    if !seen.insert(cur) {
      return None;
    }
    let kind = program.def_kind(cur)?;
    let DefKind::Import(import) = kind else {
      return Some(cur);
    };
    match import.target {
      ImportTarget::File(target_file) => {
        let (symbol, local_def) = {
          let exports = program.exports_of(target_file);
          let entry = exports.get(import.original.as_str())?;
          (entry.symbol, entry.def)
        };
        cur = program
          .symbol_info(symbol)
          .and_then(|info| info.def)
          .or(local_def)?;
      }
      _ => return None,
    }
  }
}

fn collect_module_info(program: &Program, file: FileId) -> Result<ModuleInfo, NativeJsError> {
  let lowered = program
    .hir_lowered(file)
    .ok_or_else(|| NativeJsError::MissingHirLowering {
      file: file_label(program, file),
    })?;
  let from_key = program
    .file_key(file)
    .ok_or_else(|| NativeJsError::MissingHirLowering {
      file: file_label(program, file),
    })?;

  let mut info = ModuleInfo::default();
  let mut module_requests: Vec<(u32, FileId)> = Vec::new();

  for import in &lowered.hir.imports {
    let ImportKind::Es(es) = &import.kind else {
      // `import =` is out of scope for native-js for now.
      continue;
    };

    if !is_runtime_import(es) {
      continue;
    }

    let resolved_id = program
      .resolve_module(file, &es.specifier.value)
      .ok_or_else(|| NativeJsError::UnresolvedImport {
        from: from_key.to_string(),
        specifier: es.specifier.value.clone(),
      })?;
    module_requests.push((import.span.start, resolved_id));

    // Namespace imports (`import * as ns from "./mod"`) require property access and object
    // materialization, neither of which are implemented by the minimal parse-js-driven backend.
    //
    // Default imports (`import foo from "./mod"`) are supported and are treated as importing the
    // `default` export from the target module.
    if es.namespace.is_some() {
      return Err(NativeJsError::UnsupportedImportSyntax {
        from: from_key.to_string(),
        specifier: es.specifier.value.clone(),
      });
    }

    if let Some(default) = es.default.as_ref() {
      let import_def = default
        .local_def
        .or_else(|| resolve_import_def(program, file, default.span));

      let (dep, export_name, local_name) = match import_def.and_then(|def| program.def_kind(def)) {
        Some(DefKind::Import(data)) => {
          let ImportTarget::File(mut dep) = data.target else {
            // Unresolved import; keep compiling in `project` mode.
            continue;
          };
          let mut export_name = data.original.clone();
          let local_name = program
            .def_name(import_def.expect("import def exists"))
            .unwrap_or_else(|| {
              lowered
                .names
                .resolve(default.local)
                .unwrap_or("_")
                .to_string()
            });

          // If this import points at a module that only re-exports a symbol,
          // `typecheck-ts` leaves `ExportEntry::def` empty. Follow the symbol to
          // its original defining file so we can reference the correct LLVM
          // function symbol.
          if let Some(entry) = program.exports_of(dep).get(&export_name) {
            let resolved_file = entry
              .def
              .map(|def| def.file())
              .or_else(|| program.symbol_info(entry.symbol).and_then(|i| i.file));
            if let Some(file) = resolved_file {
              if file != dep {
                if let Some((name, _)) = program
                  .exports_of(file)
                  .iter()
                  .find(|(_, candidate)| candidate.symbol == entry.symbol)
                {
                  export_name = name.clone();
                }
              }
              dep = file;
            }
          }

          (dep, export_name, local_name)
        }
        _ => {
          // Conservative fallback: rely on the HIR names, but try to resolve
          // through the target module's export map (covers re-exports where
          // `ExportEntry::def` is `None`).
          let local_name = lowered
            .names
            .resolve(default.local)
            .unwrap_or("_")
            .to_string();
          let mut export_name = "default".to_string();

          let mut dep = resolved_id;
          if let Some(entry) = program.exports_of(resolved_id).get(&export_name) {
            let resolved_file = entry
              .def
              .map(|def| def.file())
              .or_else(|| program.symbol_info(entry.symbol).and_then(|i| i.file));
            if let Some(file) = resolved_file {
              if file != dep {
                if let Some((name, _)) = program
                  .exports_of(file)
                  .iter()
                  .find(|(_, candidate)| candidate.symbol == entry.symbol)
                {
                  export_name.clone_from(name);
                }
              }
              dep = file;
            }
          }

          (dep, export_name, local_name)
        }
      };

      info.import_bindings.push(ImportBinding {
        dep,
        export_name,
        local_name,
      });
    }

    for named in es.named.iter().filter(|s| !s.is_type_only) {
      let import_def = named
        .local_def
        .or_else(|| resolve_import_def(program, file, named.span));

      let (dep, export_name, local_name) = match import_def.and_then(|def| program.def_kind(def)) {
        Some(DefKind::Import(data)) => {
          let ImportTarget::File(mut dep) = data.target else {
            continue;
          };
          let mut export_name = data.original.clone();
          let local_name = program
            .def_name(import_def.expect("def exists"))
            .unwrap_or_else(|| {
              lowered
                .names
                .resolve(named.local)
                .unwrap_or("_")
                .to_string()
            });

          // If this import points at a module that only re-exports a symbol,
          // `typecheck-ts` leaves `ExportEntry::def` empty. Follow the symbol to
          // its original defining file so we can reference the correct LLVM
          // function symbol.
          if let Some(entry) = program.exports_of(dep).get(&export_name) {
            let resolved_file = entry
              .def
              .map(|def| def.file())
              .or_else(|| program.symbol_info(entry.symbol).and_then(|i| i.file));
            if let Some(file) = resolved_file {
              if file != dep {
                if let Some((name, _)) = program
                  .exports_of(file)
                  .iter()
                  .find(|(_, candidate)| candidate.symbol == entry.symbol)
                {
                  export_name = name.clone();
                }
              }
              dep = file;
            }
          }

          (dep, export_name, local_name)
        }
        _ => {
          // Conservative fallback: rely on the HIR names, but try to resolve
          // through the target module's export map (covers re-exports where
          // `ExportEntry::def` is `None`).
          let local_name = lowered
            .names
            .resolve(named.local)
            .unwrap_or("_")
            .to_string();
          let mut export_name = lowered
            .names
            .resolve(named.imported)
            .unwrap_or("_")
            .to_string();

          let mut dep = resolved_id;
          if let Some(entry) = program.exports_of(resolved_id).get(&export_name) {
            let resolved_file = entry
              .def
              .map(|def| def.file())
              .or_else(|| program.symbol_info(entry.symbol).and_then(|i| i.file));
            if let Some(file) = resolved_file {
              if file != dep {
                if let Some((name, _)) = program
                  .exports_of(file)
                  .iter()
                  .find(|(_, candidate)| candidate.symbol == entry.symbol)
                {
                  export_name = name.clone();
                }
              }
              dep = file;
            }
          }

          (dep, export_name, local_name)
        }
      };

      info.import_bindings.push(ImportBinding {
        dep,
        export_name,
        local_name,
      });
    }
  }

  // Re-exports like `export { foo } from "./dep"` and `export * from "./dep"`
  // must also be treated as runtime dependencies so the referenced module's
  // initializer runs.
  //
  // Type-only re-exports are erased from JS output and therefore must *not*
  // trigger module evaluation.
  for export in &lowered.hir.exports {
    match &export.kind {
      hir_js::ExportKind::Named(named) => {
        let Some(source) = named.source.as_ref() else {
          continue;
        };
        if named.is_type_only {
          continue;
        }
        let has_value_specifiers = named.specifiers.iter().any(|s| !s.is_type_only);
        let is_side_effect_export = named.specifiers.is_empty();
        if !has_value_specifiers && !is_side_effect_export {
          continue;
        }

        let resolved_id = program.resolve_module(file, &source.value).ok_or_else(|| {
          NativeJsError::UnresolvedImport {
            from: from_key.to_string(),
            specifier: source.value.clone(),
          }
        })?;
        module_requests.push((export.span.start, resolved_id));
      }
      hir_js::ExportKind::ExportAll(all) => {
        if all.is_type_only {
          continue;
        }
        let resolved_id = program
          .resolve_module(file, &all.source.value)
          .ok_or_else(|| NativeJsError::UnresolvedImport {
            from: from_key.to_string(),
            specifier: all.source.value.clone(),
          })?;
        module_requests.push((export.span.start, resolved_id));
      }
      _ => {}
    }
  }

  module_requests.sort_by_key(|(start, _)| *start);
  let mut seen = BTreeSet::<FileId>::new();
  for (_, dep) in module_requests {
    if seen.insert(dep) {
      info.deps.push(dep);
    }
  }

  info.import_bindings.sort_by(|a, b| {
    a.dep
      .0
      .cmp(&b.dep.0)
      .then(a.export_name.cmp(&b.export_name))
      .then(a.local_name.cmp(&b.local_name))
  });

  Ok(info)
}

fn runtime_reachable(entry: FileId, modules: &BTreeMap<FileId, ModuleInfo>) -> BTreeSet<FileId> {
  let mut seen = BTreeSet::new();
  let mut queue = VecDeque::new();
  seen.insert(entry);
  queue.push_back(entry);
  while let Some(file) = queue.pop_front() {
    let Some(info) = modules.get(&file) else {
      continue;
    };
    for dep in &info.deps {
      if seen.insert(*dep) {
        queue.push_back(*dep);
      }
    }
  }
  seen
}

fn topo_sort_runtime(
  program: &Program,
  entry: FileId,
  nodes: &BTreeSet<FileId>,
  modules: &BTreeMap<FileId, ModuleInfo>,
) -> Result<Vec<FileId>, NativeJsError> {
  // ECMAScript module evaluation order is a DFS: evaluate dependencies in source order,
  // then evaluate the module itself. This preserves sibling `import` ordering.
  let mut stack: Vec<FileId> = Vec::new();
  let mut visiting: BTreeSet<FileId> = BTreeSet::new();
  let mut visited: BTreeSet<FileId> = BTreeSet::new();
  let mut out: Vec<FileId> = Vec::with_capacity(nodes.len());

  fn dfs(
    program: &Program,
    node: FileId,
    nodes: &BTreeSet<FileId>,
    modules: &BTreeMap<FileId, ModuleInfo>,
    stack: &mut Vec<FileId>,
    visiting: &mut BTreeSet<FileId>,
    visited: &mut BTreeSet<FileId>,
    out: &mut Vec<FileId>,
  ) -> Result<(), NativeJsError> {
    if visited.contains(&node) {
      return Ok(());
    }
    if visiting.contains(&node) {
      let idx = stack.iter().position(|f| *f == node).unwrap_or(0);
      let mut cycle = stack[idx..].to_vec();
      cycle.push(node);
      let mut formatted: Vec<String> = cycle.iter().map(|f| file_label(program, *f)).collect();
      if formatted.is_empty() {
        formatted.push("<unknown cycle>".into());
      }
      return Err(NativeJsError::ModuleCycle {
        cycle: formatted.join(" -> "),
      });
    }

    visiting.insert(node);
    stack.push(node);

    if let Some(info) = modules.get(&node) {
      for dep in &info.deps {
        if nodes.contains(dep) {
          dfs(program, *dep, nodes, modules, stack, visiting, visited, out)?;
        }
      }
    }

    stack.pop();
    visiting.remove(&node);
    visited.insert(node);
    out.push(node);
    Ok(())
  }

  dfs(
    program,
    entry,
    nodes,
    modules,
    &mut stack,
    &mut visiting,
    &mut visited,
    &mut out,
  )?;

  Ok(out)
}

fn sanitize_llvm_component(name: &str) -> String {
  let mut out = String::new();
  for ch in name.chars() {
    if ch.is_ascii_alphanumeric() || ch == '_' {
      out.push(ch);
    } else {
      out.push('_');
    }
  }
  if out.is_empty() {
    out.push('_');
  }
  out
}

fn llvm_fn_symbol(file: FileId, local_name: &str) -> String {
  format!("@njs_f_{}_{}", file.0, sanitize_llvm_component(local_name))
}

fn llvm_init_symbol(file: FileId) -> String {
  format!("@njs_init_{}", file.0)
}

pub fn compile_project_to_llvm_ir(
  program: &Program,
  host: &dyn Host,
  entry_file: FileId,
  opts: CompileOptions,
  entry_export: Option<&str>,
) -> Result<String, NativeJsError> {
  let all_files = program.reachable_files();

  let mut modules: BTreeMap<FileId, ModuleInfo> = BTreeMap::new();
  for file in all_files.iter().copied() {
    let _ = program.file_body(file);
    let info = collect_module_info(program, file)?;
    modules.insert(file, info);
  }

  let runtime_files = runtime_reachable(entry_file, &modules);
  let init_order = topo_sort_runtime(program, entry_file, &runtime_files, &modules)?;

  // Parse all reachable files (even those that are only type-reachable) so we can compile their
  // local function definitions and initializer bodies deterministically.
  let mut asts: BTreeMap<FileId, Node<parse_js::ast::stx::TopLevel>> = BTreeMap::new();
  let mut local_fn_sigs: BTreeMap<FileId, BTreeMap<String, (Vec<Ty>, Ty)>> = BTreeMap::new();
  let mut sources: BTreeMap<FileId, Arc<str>> = BTreeMap::new();
  for file in all_files.iter().copied() {
    let source = program
      .file_text(file)
      .ok_or_else(|| NativeJsError::FileText {
        file: file_label(program, file),
        reason: "Program::file_text returned None".into(),
      })?;
    sources.insert(file, source.clone());

    let key = file_key_or_fallback(program, file);
    let kind = host.file_kind(&key);
    let parsed = parse_with_options(
      &source,
      ParseOptions {
        dialect: dialect_for_kind(kind),
        source_type: SourceType::Module,
      },
    )
    .map_err(|error| NativeJsError::ParseFile {
      file: file_label(program, file),
      file_id: file,
      error,
    })?;

    let mut fn_map: BTreeMap<String, (Vec<Ty>, Ty)> = BTreeMap::new();
    for stmt in &parsed.stx.body {
      if let Stmt::FunctionDecl(func) = stmt.stx.as_ref() {
        let Some(name) = func.stx.name.as_ref().map(|n| n.stx.name.clone()) else {
          continue;
        };
        // Ignore overload signatures (they have no runtime body).
        if func.stx.function.stx.body.is_none() {
          continue;
        }

        let mut params = Vec::new();
        for param in &func.stx.function.stx.parameters {
          if param.stx.rest || param.stx.optional {
            return Err(NativeJsError::CodegenFile {
              file: file_label(program, file),
              file_id: file,
              error: CodegenError::TypeError {
                message: format!("function `{name}` has unsupported parameter syntax"),
                loc: param.loc,
              },
            });
          }
          params.push(
            ty_from_opt_type_expr(param.stx.type_annotation.as_ref()).map_err(|err| match err {
              NativeJsError::Codegen(error) => NativeJsError::CodegenFile {
                file: file_label(program, file),
                file_id: file,
                error,
              },
              other => other,
            })?,
          );
        }
        let ret =
          ty_from_opt_type_expr(func.stx.function.stx.return_type.as_ref()).map_err(|err| match err {
            NativeJsError::Codegen(error) => NativeJsError::CodegenFile {
              file: file_label(program, file),
              file_id: file,
              error,
            },
            other => other,
          })?;
        fn_map.insert(name, (params, ret));
      }
    }

    local_fn_sigs.insert(file, fn_map);
    asts.insert(file, parsed);
  }

  // If the caller didn't specify an entry export, try to auto-call an exported `main()` if it
  // exists and is a supported local function with no parameters.
  //
  // This keeps `native-js-cli` ergonomics close to "run my project" while staying conservative:
  // we only auto-call when the export resolves to a local function declaration with a body.
  let entry_export = entry_export.or_else(|| {
    let def = resolve_export_def(program, entry_file, "main")?;
    let local = program.def_name(def)?;
    let (params, _ret) = local_fn_sigs.get(&def.file()).and_then(|m| m.get(&local))?;
    params.is_empty().then_some("main")
  });

  // Build call target tables per file.
  let mut call_targets: BTreeMap<FileId, BTreeMap<String, UserFunctionSig>> = BTreeMap::new();
  for file in all_files.iter().copied() {
    let mut table: BTreeMap<String, UserFunctionSig> = BTreeMap::new();
    if let Some(funcs) = local_fn_sigs.get(&file) {
      for (name, (params, ret)) in funcs {
        table.insert(
          name.clone(),
          UserFunctionSig {
            llvm_name: llvm_fn_symbol(file, name),
            ret: *ret,
            params: params.clone(),
          },
        );
      }
    }

    if let Some(info) = modules.get(&file) {
      for binding in &info.import_bindings {
        let export_map = program.exports_of(binding.dep);
        if !export_map.contains_key(&binding.export_name) {
          return Err(NativeJsError::MissingExport {
            file: file_label(program, binding.dep),
            export: binding.export_name.clone(),
          });
        }

        let def =
          resolve_export_def(program, binding.dep, &binding.export_name).ok_or_else(|| {
            NativeJsError::UnsupportedExport {
              file: file_label(program, binding.dep),
              export: binding.export_name.clone(),
            }
          })?;
        let local = program
          .def_name(def)
          .ok_or_else(|| NativeJsError::UnsupportedExport {
            file: file_label(program, binding.dep),
            export: binding.export_name.clone(),
          })?;
        let (params, ret) = local_fn_sigs
          .get(&def.file())
          .and_then(|m| m.get(&local))
          .cloned()
          .ok_or_else(|| NativeJsError::UnsupportedExport {
            file: file_label(program, binding.dep),
            export: binding.export_name.clone(),
          })?;
        table.insert(
          binding.local_name.clone(),
          UserFunctionSig {
            llvm_name: llvm_fn_symbol(def.file(), &local),
            ret,
            params,
          },
        );
      }
    }

    call_targets.insert(file, table);
  }

  // If an entry function was requested, resolve it to a concrete LLVM symbol.
  let entry_call: Option<UserFunctionSig> = if let Some(entry_export) = entry_export {
    let export_map = program.exports_of(entry_file);
    if !export_map.contains_key(entry_export) {
      return Err(NativeJsError::MissingExport {
        file: file_label(program, entry_file),
        export: entry_export.to_string(),
      });
    }

    let def = resolve_export_def(program, entry_file, entry_export).ok_or_else(|| {
      NativeJsError::UnsupportedExport {
        file: file_label(program, entry_file),
        export: entry_export.to_string(),
      }
    })?;
    let local = program
      .def_name(def)
      .ok_or_else(|| NativeJsError::UnsupportedExport {
        file: file_label(program, def.file()),
        export: entry_export.to_string(),
      })?;
    let (params, ret) = local_fn_sigs
      .get(&def.file())
      .and_then(|m| m.get(&local))
      .cloned()
      .ok_or_else(|| NativeJsError::UnsupportedExport {
        file: file_label(program, def.file()),
        export: entry_export.to_string(),
      })?;
    if !params.is_empty() {
      return Err(NativeJsError::UnsupportedExport {
        file: file_label(program, def.file()),
        export: entry_export.to_string(),
      });
    }
    Some(UserFunctionSig {
      llvm_name: llvm_fn_symbol(def.file(), &local),
      ret,
      params,
    })
  } else {
    None
  };

  // Emit LLVM IR.
  let mut builder = LlvmModuleBuilder::new(opts);
  let entry_key = file_key_or_fallback(program, entry_file);
  let entry_source = sources
    .get(&entry_file)
    .expect("entry file source should be loaded");
  builder.set_entry_file(entry_key.as_str(), entry_source);

  for file in all_files.iter().copied() {
    let key = file_key_or_fallback(program, file);
    let source = sources.get(&file).expect("source loaded for file");
    builder.set_source_file(key.as_str(), source);

    let ast = asts.get(&file).expect("AST parsed for file");
    let targets = call_targets.get(&file).expect("call targets for file");

    // Compile initializer: runtime statements excluding imports/exports and type-only decls.
    let mut init_stmts: Vec<&Node<Stmt>> = Vec::new();
    for stmt in &ast.stx.body {
      match stmt.stx.as_ref() {
        Stmt::Import(_)
        | Stmt::ExportList(_)
        | Stmt::ExportDefaultExpr(_)
        | Stmt::ExportAssignmentDecl(_)
        | Stmt::ExportAsNamespaceDecl(_)
        | Stmt::ExportTypeDecl(_)
        | Stmt::ImportTypeDecl(_)
        | Stmt::ImportEqualsDecl(_)
        | Stmt::FunctionDecl(_)
        | Stmt::InterfaceDecl(_)
        | Stmt::TypeAliasDecl(_)
        | Stmt::EnumDecl(_)
        | Stmt::NamespaceDecl(_)
        | Stmt::ModuleDecl(_)
        | Stmt::GlobalDecl(_)
        | Stmt::AmbientVarDecl(_)
        | Stmt::AmbientFunctionDecl(_)
        | Stmt::AmbientClassDecl(_) => {}
        _ => init_stmts.push(stmt),
      }
    }

    builder
      .add_init_function(&llvm_init_symbol(file), &init_stmts, targets)
      .map_err(|error| NativeJsError::CodegenFile {
        file: file_label(program, file),
        file_id: file,
        error,
      })?;

    // Compile top-level function declarations.
    for stmt in &ast.stx.body {
      let Stmt::FunctionDecl(func) = stmt.stx.as_ref() else {
        continue;
      };
      let Some(name) = func.stx.name.as_ref().map(|n| n.stx.name.as_str()) else {
        continue;
      };
      if func.stx.function.stx.body.is_none() {
        continue;
      }
      let sig = targets
        .get(name)
        .ok_or_else(|| NativeJsError::UnsupportedExport {
          file: file_label(program, file),
          export: name.to_string(),
        })?;
      builder
        .add_ts_function(&sig.llvm_name, func, targets)
        .map_err(|error| NativeJsError::CodegenFile {
          file: file_label(program, file),
          file_id: file,
          error,
        })?;
    }
  }

  // Build `main`: run initializers in topo order (runtime graph only), then optionally invoke the
  // configured entry function.
  let init_symbols: Vec<String> = init_order.iter().copied().map(llvm_init_symbol).collect();
  // `main` is synthetic; tie it to the entry file for debugging.
  builder.set_source_file(entry_key.as_str(), entry_source);
  builder
    .add_main(&init_symbols, entry_call.as_ref())
    .map_err(|error| NativeJsError::CodegenFile {
      file: file_label(program, entry_file),
      file_id: entry_file,
      error,
    })?;

  Ok(builder.finish())
}
