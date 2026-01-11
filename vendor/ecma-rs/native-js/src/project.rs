use crate::codegen::llvm::{LlvmModuleBuilder, Ty, UserFunctionSig};
use crate::codegen::CodegenError;
use crate::{CompileOptions, NativeJsError};
use hir_js::ImportKind;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::type_expr::TypeExpr;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use typecheck_ts::{DefId, DefKind, FileId, FileKey, Host, ImportTarget, Program, TextRange};

#[derive(Debug, Default, Clone)]
struct ModuleInfo {
  /// Runtime dependencies (value imports + side-effect imports).
  deps: BTreeSet<FileId>,
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
    other => Err(NativeJsError::Codegen(CodegenError::TypeError(format!(
      "unsupported type annotation: {other:?}"
    )))),
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

fn collect_module_info(
  program: &Program,
  host: &dyn Host,
  file: FileId,
) -> Result<ModuleInfo, NativeJsError> {
  let lowered = program.hir_lowered(file).ok_or_else(|| NativeJsError::MissingHirLowering {
    file: file_label(program, file),
  })?;
  let from_key = program.file_key(file).ok_or_else(|| NativeJsError::MissingHirLowering {
    file: file_label(program, file),
  })?;

  let mut info = ModuleInfo::default();

  for import in &lowered.hir.imports {
    let ImportKind::Es(es) = &import.kind else {
      // `import =` is out of scope for native-js for now.
      continue;
    };

    if !is_runtime_import(es) {
      continue;
    }

    let resolved = host
      .resolve(&from_key, &es.specifier.value)
      .ok_or_else(|| NativeJsError::UnresolvedImport {
        from: from_key.to_string(),
        specifier: es.specifier.value.clone(),
      })?;

    let resolved_id = program.file_id(&resolved).ok_or_else(|| NativeJsError::UnresolvedImport {
      from: from_key.to_string(),
      specifier: es.specifier.value.clone(),
    })?;

    info.deps.insert(resolved_id);

    // Only `import { foo } from "..."` is supported for now.
    if es.default.is_some() || es.namespace.is_some() {
      return Err(NativeJsError::UnresolvedImport {
        from: from_key.to_string(),
        specifier: es.specifier.value.clone(),
      });
    }

    for named in es.named.iter().filter(|s| !s.is_type_only) {
      let import_def = named
        .local_def
        .or_else(|| resolve_import_def(program, file, named.span));

      let (dep, export_name, local_name) = match import_def.and_then(|def| program.def_kind(def)) {
        Some(DefKind::Import(data)) => {
          let ImportTarget::File(dep) = data.target else {
            continue;
          };
          let local_name = program
            .def_name(import_def.expect("def exists"))
            .unwrap_or_else(|| {
              lowered
                .names
                .resolve(named.local)
                .unwrap_or("_")
                .to_string()
            });
          (dep, data.original.clone(), local_name)
        }
        _ => {
          // Conservative fallback: rely on the HIR names + resolved module file.
          let local = lowered
            .names
            .resolve(named.local)
            .unwrap_or("_")
            .to_string();
          let imported = lowered
            .names
            .resolve(named.imported)
            .unwrap_or("_")
            .to_string();
          (resolved_id, imported, local)
        }
      };

      info.import_bindings.push(ImportBinding {
        dep,
        export_name,
        local_name,
      });
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
  nodes: &BTreeSet<FileId>,
  modules: &BTreeMap<FileId, ModuleInfo>,
) -> Result<Vec<FileId>, NativeJsError> {
  let mut outgoing: BTreeMap<FileId, Vec<FileId>> = BTreeMap::new();
  let mut indegree: BTreeMap<FileId, usize> = BTreeMap::new();

  for file in nodes {
    indegree.insert(*file, 0);
  }

  // Edge orientation: dep -> user, so deps come first in topo order.
  for file in nodes {
    let Some(info) = modules.get(file) else {
      continue;
    };
    for dep in &info.deps {
      if !nodes.contains(dep) {
        continue;
      }
      outgoing.entry(*dep).or_default().push(*file);
      *indegree.entry(*file).or_default() += 1;
    }
  }

  for users in outgoing.values_mut() {
    users.sort_by_key(|id| id.0);
    users.dedup();
  }

  let mut ready: BTreeSet<FileId> = indegree
    .iter()
    .filter_map(|(file, deg)| (*deg == 0).then_some(*file))
    .collect();

  let mut order = Vec::with_capacity(nodes.len());
  while let Some(file) = ready.pop_first() {
    order.push(file);
    if let Some(users) = outgoing.get(&file) {
      for user in users {
        let deg = indegree
          .get_mut(user)
          .expect("node present in indegree map");
        *deg = deg.saturating_sub(1);
        if *deg == 0 {
          ready.insert(*user);
        }
      }
    }
  }

  if order.len() != nodes.len() {
    let cycle = find_cycle(nodes, modules).unwrap_or_default();
    let mut formatted: Vec<String> = cycle.iter().map(|f| file_label(program, *f)).collect();
    if formatted.is_empty() {
      formatted.push("<unknown cycle>".into());
    }
    return Err(NativeJsError::ModuleCycle {
      cycle: formatted.join(" -> "),
    });
  }

  Ok(order)
}

fn find_cycle(nodes: &BTreeSet<FileId>, modules: &BTreeMap<FileId, ModuleInfo>) -> Option<Vec<FileId>> {
  #[derive(Clone, Copy, PartialEq, Eq)]
  enum Mark {
    Temp,
    Perm,
  }

  fn dfs(
    node: FileId,
    stack: &mut Vec<FileId>,
    marks: &mut BTreeMap<FileId, Mark>,
    nodes: &BTreeSet<FileId>,
    modules: &BTreeMap<FileId, ModuleInfo>,
  ) -> Option<Vec<FileId>> {
    marks.insert(node, Mark::Temp);
    stack.push(node);

    let mut deps: Vec<FileId> = modules
      .get(&node)
      .map(|m| m.deps.iter().copied().filter(|d| nodes.contains(d)).collect())
      .unwrap_or_default();
    deps.sort_by_key(|id| id.0);
    deps.dedup();

    for dep in deps {
      match marks.get(&dep) {
        Some(Mark::Temp) => {
          let idx = stack.iter().position(|f| *f == dep).unwrap_or(0);
          let mut cycle = stack[idx..].to_vec();
          cycle.push(dep);
          return Some(cycle);
        }
        Some(Mark::Perm) => continue,
        None => {
          if let Some(cycle) = dfs(dep, stack, marks, nodes, modules) {
            return Some(cycle);
          }
        }
      }
    }

    stack.pop();
    marks.insert(node, Mark::Perm);
    None
  }

  let mut marks: BTreeMap<FileId, Mark> = BTreeMap::new();
  let mut stack = Vec::new();
  for node in nodes.iter().copied() {
    if marks.contains_key(&node) {
      continue;
    }
    if let Some(cycle) = dfs(node, &mut stack, &mut marks, nodes, modules) {
      return Some(cycle);
    }
  }
  None
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
    let info = collect_module_info(program, host, file)?;
    modules.insert(file, info);
  }

  let runtime_files = runtime_reachable(entry_file, &modules);
  let init_order = topo_sort_runtime(program, &runtime_files, &modules)?;

  // Parse all reachable files (even those that are only type-reachable) so we can compile their
  // local function definitions and initializer bodies deterministically.
  let mut asts: BTreeMap<FileId, Node<parse_js::ast::stx::TopLevel>> = BTreeMap::new();
  let mut local_fn_sigs: BTreeMap<FileId, BTreeMap<String, (Vec<Ty>, Ty)>> = BTreeMap::new();
  for file in all_files.iter().copied() {
    let source = program.file_text(file).ok_or_else(|| NativeJsError::FileText {
      file: file_label(program, file),
      reason: "Program::file_text returned None".into(),
    })?;

    let key = file_key_or_fallback(program, file);
    let kind = host.file_kind(&key);
    let parsed = parse_with_options(
      &source,
      ParseOptions {
        dialect: dialect_for_kind(kind),
        source_type: SourceType::Module,
      },
    )?;

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
            return Err(NativeJsError::Codegen(CodegenError::TypeError(format!(
              "function `{name}` has unsupported parameter syntax"
            ))));
          }
          params.push(ty_from_opt_type_expr(param.stx.type_annotation.as_ref())?);
        }
        let ret = ty_from_opt_type_expr(func.stx.function.stx.return_type.as_ref())?;
        fn_map.insert(name, (params, ret));
      }
    }

    local_fn_sigs.insert(file, fn_map);
    asts.insert(file, parsed);
  }

  // Compute which exports must be materialized for runtime imports + the configured entrypoint.
  let mut used_exports: BTreeMap<FileId, BTreeSet<String>> = BTreeMap::new();
  if let Some(entry_export) = entry_export {
    used_exports
      .entry(entry_file)
      .or_default()
      .insert(entry_export.to_string());
  }
  for file in all_files.iter().copied() {
    let Some(info) = modules.get(&file) else {
      continue;
    };
    for binding in &info.import_bindings {
      used_exports
        .entry(binding.dep)
        .or_default()
        .insert(binding.export_name.clone());
    }
  }

  let mut export_locals: BTreeMap<FileId, BTreeMap<String, String>> = BTreeMap::new();
  for (file, exports) in &used_exports {
    let export_map = program.exports_of(*file);
    for export_name in exports {
      let entry = export_map.get(export_name).ok_or_else(|| NativeJsError::MissingExport {
        file: file_label(program, *file),
        export: export_name.clone(),
      })?;
      let def = entry.def.ok_or_else(|| NativeJsError::UnsupportedExport {
        file: file_label(program, *file),
        export: export_name.clone(),
      })?;
      let local = program
        .def_name(def)
        .ok_or_else(|| NativeJsError::UnsupportedExport {
          file: file_label(program, *file),
          export: export_name.clone(),
        })?;
      export_locals
        .entry(*file)
        .or_default()
        .insert(export_name.clone(), local);
    }
  }

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
        let target_local = export_locals
          .get(&binding.dep)
          .and_then(|m| m.get(&binding.export_name))
          .ok_or_else(|| NativeJsError::MissingExport {
            file: file_label(program, binding.dep),
            export: binding.export_name.clone(),
          })?;
        let (params, ret) = local_fn_sigs
          .get(&binding.dep)
          .and_then(|m| m.get(target_local))
          .cloned()
          .ok_or_else(|| NativeJsError::UnsupportedExport {
            file: file_label(program, binding.dep),
            export: binding.export_name.clone(),
          })?;
        table.insert(
          binding.local_name.clone(),
          UserFunctionSig {
            llvm_name: llvm_fn_symbol(binding.dep, target_local),
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
    let local = export_locals
      .get(&entry_file)
      .and_then(|m| m.get(entry_export))
      .ok_or_else(|| NativeJsError::MissingExport {
        file: file_label(program, entry_file),
        export: entry_export.to_string(),
      })?;
    let (params, ret) = local_fn_sigs
      .get(&entry_file)
      .and_then(|m| m.get(local))
      .cloned()
      .ok_or_else(|| NativeJsError::UnsupportedExport {
        file: file_label(program, entry_file),
        export: entry_export.to_string(),
      })?;
    if !params.is_empty() {
      return Err(NativeJsError::UnsupportedExport {
        file: file_label(program, entry_file),
        export: entry_export.to_string(),
      });
    }
    Some(UserFunctionSig {
      llvm_name: llvm_fn_symbol(entry_file, local),
      ret,
      params,
    })
  } else {
    None
  };

  // Emit LLVM IR.
  let mut builder = LlvmModuleBuilder::new(opts);

  for file in all_files.iter().copied() {
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
      .map_err(NativeJsError::Codegen)?;

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
      let sig = targets.get(name).ok_or_else(|| NativeJsError::UnsupportedExport {
        file: file_label(program, file),
        export: name.to_string(),
      })?;
      builder
        .add_ts_function(&sig.llvm_name, func, targets)
        .map_err(NativeJsError::Codegen)?;
    }
  }

  // Build `main`: run initializers in topo order (runtime graph only), then optionally invoke the
  // configured entry function.
  let init_symbols: Vec<String> = init_order.iter().copied().map(llvm_init_symbol).collect();
  builder
    .add_main(&init_symbols, entry_call.as_ref())
    .map_err(NativeJsError::Codegen)?;

  Ok(builder.finish())
}
