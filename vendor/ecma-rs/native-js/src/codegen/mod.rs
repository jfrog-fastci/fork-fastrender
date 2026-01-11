//! Native code generation backends for `native-js`.
//!
//! This module currently contains:
//! - `emit_llvm_module`: a minimal `parse-js`-driven LLVM IR emitter (used by
//!   `compile_typescript_to_llvm_ir`; `native-js-cli` uses the related multi-file
//!   `compile_project_to_llvm_ir` entrypoint).
//! - [`codegen`]: an experimental HIR-driven backend used by the typechecked
//!   pipeline (`native-js-cli --pipeline checked` and `native-js-cli --bin native-js`).
//!
//! ## Diagnostic codes
//!
//! The HIR backend emits stable `NJS01xx` codes for codegen failures:
//!
//! - `NJS0100`: failed to access lowered HIR for entry file
//! - `NJS0101`: failed to access lowered HIR for a function body / failed to locate `main` for codegen
//! - `NJS0102`: missing function metadata
//! - `NJS0103`: expression id out of bounds
//! - `NJS0104`: numeric literal cannot be represented as a 32-bit integer
//! - `NJS0105`: unsupported unary operator
//! - `NJS0106`: unsupported binary operator
//! - `NJS0107`: unsupported expression in the current codegen subset
//! - `NJS0112`: statement id out of bounds
//! - `NJS0113`: unsupported statement / variable declaration kind in the current codegen subset
//! - `NJS0114`: use of unknown/unbound identifier in the current codegen subset
//! - `NJS0115`: not all control-flow paths return a value in the current codegen subset
//! - `NJS0116`: `return;` is only supported when `main` returns `void`/`undefined`
//! - `NJS0118`: variable declarations must have an initializer
//! - `NJS0119`: unknown loop label for `break`
//! - `NJS0120`: `break` is only supported inside loops
//! - `NJS0121`: unknown loop label for `continue`
//! - `NJS0122`: `continue` is only supported inside loops (also used for unsupported binding patterns)
//! - `NJS0123`: failed to resolve call signature for exported `main`
//! - `NJS0124`: only labeled loops are supported in native-js codegen
//! - `NJS0130`: failed to resolve identifier/callee during codegen
//! - `NJS0132`: unsupported assignment target
//! - `NJS0134`: unsupported assignment operator
//! - `NJS0140`: failed to resolve definition kind for a global/import binding
//! - `NJS0141`: unresolved import binding (or cyclic import resolution)
//! - `NJS0142`: unsupported global binding kind in codegen
//! - `NJS0144`: unsupported call syntax in codegen subset
//! - `NJS0145`: call to unknown function (or void call not supported)
//!
//! Entrypoint-related errors are emitted by [`crate::strict::entrypoint`]
//! (`NJS0108..NJS0111`).
use crate::resolve::BindingId;
use crate::strict::Entrypoint;
use crate::Resolver;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{
  AssignOp, BinaryOp, ExprId, ExprKind, FileKind, ForInit, ImportKind, Literal, NameId, PatKind,
  StmtId, StmtKind, UnaryOp, UpdateOp, VarDecl, VarDeclKind,
};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::IntType;
use inkwell::values::{FunctionValue, GlobalValue, IntValue, PointerValue};
use inkwell::AddressSpace;
use inkwell::IntPredicate;
use std::collections::{HashMap, HashSet, VecDeque};
use typecheck_ts::{DefId, FileId, Program, TypeKindSummary};

pub struct CodegenOptions {
  pub module_name: String,
}

impl Default for CodegenOptions {
  fn default() -> Self {
    Self {
      module_name: "native_js".to_string(),
    }
  }
}

pub fn codegen<'ctx>(
  context: &'ctx Context,
  program: &Program,
  entry_file: FileId,
  entrypoint: Entrypoint,
  options: CodegenOptions,
) -> Result<Module<'ctx>, Vec<Diagnostic>> {
  let mut cg = ProgramCodegen::new(context, program, &options.module_name);
  cg.compile(entry_file, entrypoint)?;
  Ok(cg.finish())
}

fn main_allows_void_return(
  program: &Program,
  main_def: DefId,
  entry_file: FileId,
) -> Result<bool, Vec<Diagnostic>> {
  let func_ty = program.type_of_def_interned(main_def);
  let sigs = program.call_signatures(func_ty);
  let Some(sig) = sigs.first() else {
    return Err(vec![Diagnostic::error(
      "NJS0123",
      "failed to resolve call signature for exported `main`",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  };
  let ret_kind = program.type_kind(sig.signature.ret);
  Ok(matches!(
    ret_kind,
    TypeKindSummary::Void | TypeKindSummary::Undefined
  ))
}

struct ProgramCodegen<'ctx, 'p> {
  context: &'ctx Context,
  module: Module<'ctx>,
  i32_ty: IntType<'ctx>,
  bool_ty: IntType<'ctx>,
  program: &'p Program,
  resolver: Resolver<'p>,
  exported_defs: HashSet<DefId>,
  globals: HashMap<DefId, GlobalValue<'ctx>>,
  functions: HashMap<DefId, FunctionValue<'ctx>>,
  file_inits: HashMap<FileId, FunctionValue<'ctx>>,
}

impl<'ctx, 'p> ProgramCodegen<'ctx, 'p> {
  fn new(context: &'ctx Context, program: &'p Program, module_name: &str) -> Self {
    Self {
      context,
      module: context.create_module(module_name),
      i32_ty: context.i32_type(),
      bool_ty: context.bool_type(),
      program,
      resolver: Resolver::new(program),
      exported_defs: HashSet::new(),
      globals: HashMap::new(),
      functions: HashMap::new(),
      file_inits: HashMap::new(),
    }
  }

  fn finish(self) -> Module<'ctx> {
    self.module
  }

  fn compile(&mut self, entry_file: FileId, entrypoint: Entrypoint) -> Result<(), Vec<Diagnostic>> {
    let Some(lowered) = self.program.hir_lowered(entry_file) else {
      return Err(vec![Diagnostic::error(
        "NJS0100",
        "failed to access lowered HIR for entry file",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]);
    };
    if matches!(lowered.hir.file_kind, FileKind::Dts) {
      return Err(vec![Diagnostic::error(
        "NJS0100",
        "entry file must not be a declaration file",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]);
    }

    let main_def = entrypoint.main_def;
    let allow_void_main_return = main_allows_void_return(self.program, main_def, entry_file)?;

    let files = self.runtime_files(entry_file);
    self.collect_exported_defs(&files);
    let init_order = self.init_order(entry_file, &files);

    for file in &files {
      self.ensure_file_init(*file);
    }

    // Predeclare all top-level functions (so calls can reference them).
    let mut function_bodies: Vec<(DefId, FileId, hir_js::BodyId)> = Vec::new();
    for file in &files {
      let Some(lowered) = self.program.hir_lowered(*file) else {
        continue;
      };
      for def in &lowered.defs {
        if def.parent.is_some() {
          continue;
        }
        if def.path.kind != hir_js::DefKind::Function {
          continue;
        }
        let Some(body_id) = def.body else {
          continue;
        };
        let Some(body) = lowered.body(body_id) else {
          continue;
        };
        let Some(function) = body.function.as_ref() else {
          continue;
        };
        self.ensure_ts_function(def.id, function.params.len());
        function_bodies.push((def.id, *file, body_id));
      }
    }

    let Some(main_fn) = self.functions.get(&main_def).copied() else {
      return Err(vec![Diagnostic::error(
        "NJS0101",
        "failed to locate exported `main` function for codegen",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]);
    };

    // Define all file init functions.
    for file in &files {
      self.build_file_init(*file)?;
    }

    // Define all top-level functions.
    for (def, file, body_id) in function_bodies {
      let allow_void_return = def == main_def && allow_void_main_return;
      self.build_ts_function(def, file, body_id, allow_void_return)?;
    }

    // Build C entrypoint wrapper that runs module initializers and then calls TS main.
    self.build_c_main(main_fn, &init_order);

    Ok(())
  }

  fn runtime_files(&self, entry_file: FileId) -> Vec<FileId> {
    let mut visited: HashSet<FileId> = HashSet::new();
    let mut queue: VecDeque<FileId> = VecDeque::new();
    queue.push_back(entry_file);

    while let Some(file) = queue.pop_front() {
      if visited.contains(&file) {
        continue;
      }
      let Some(lowered) = self.program.hir_lowered(file) else {
        continue;
      };
      if matches!(lowered.hir.file_kind, FileKind::Dts) {
        continue;
      }
      visited.insert(file);
      for dep in file_import_deps(self.program, &lowered) {
        queue.push_back(dep);
      }
    }

    let mut files: Vec<FileId> = visited.into_iter().collect();
    files.sort_by_key(|id| id.0);
    files
  }

  fn collect_exported_defs(&mut self, files: &[FileId]) {
    self.exported_defs.clear();
    for file in files {
      for entry in self.program.exports_of(*file).values() {
        if let Some(def) = entry.def {
          self.exported_defs.insert(def);
        }
      }
    }
  }

  fn init_order(&self, entry_file: FileId, files: &[FileId]) -> Vec<FileId> {
    let file_set: HashSet<FileId> = files.iter().copied().collect();
    let mut deps: HashMap<FileId, Vec<FileId>> = HashMap::new();
    for file in files {
      let Some(lowered) = self.program.hir_lowered(*file) else {
        continue;
      };
      let out: Vec<FileId> = file_import_deps(self.program, &lowered)
        .into_iter()
        .filter(|dep| file_set.contains(dep))
        .collect();
      deps.insert(*file, out);
    }

    let mut visited = HashSet::<FileId>::new();
    let mut visiting = HashSet::<FileId>::new();
    let mut order = Vec::new();
    topo_visit(entry_file, &deps, &mut visited, &mut visiting, &mut order);
    order
  }

  fn ensure_file_init(&mut self, file: FileId) {
    if self.file_inits.contains_key(&file) {
      return;
    }
    let name = crate::llvm_symbol_for_file_init(file);
    let fn_ty = self.context.void_type().fn_type(&[], false);
    let func = self.module.add_function(&name, fn_ty, Some(Linkage::Internal));
    crate::stack_walking::apply_stack_walking_attrs(self.context, func);
    self.file_inits.insert(file, func);
  }

  fn ensure_ts_function(&mut self, def: DefId, param_count: usize) {
    if self.functions.contains_key(&def) {
      return;
    }
    let name = crate::llvm_symbol_for_def(self.program, def);
    let params: Vec<_> = std::iter::repeat(self.i32_ty.into())
      .take(param_count)
      .collect();
    let fn_ty = self.i32_ty.fn_type(&params, false);
    let linkage = if self.exported_defs.contains(&def) {
      None
    } else {
      Some(Linkage::Internal)
    };
    let func = self.module.add_function(&name, fn_ty, linkage);
    crate::stack_walking::apply_stack_walking_attrs(self.context, func);
    self.functions.insert(def, func);
  }

  fn build_file_init(&mut self, file: FileId) -> Result<(), Vec<Diagnostic>> {
    let Some(func) = self.file_inits.get(&file).copied() else {
      return Ok(());
    };
    if func.get_first_basic_block().is_some() {
      return Ok(());
    }
    let Some(lowered) = self.program.hir_lowered(file) else {
      return Ok(());
    };
    let Some(body) = lowered.body(lowered.root_body()) else {
      return Ok(());
    };

    let mut cg = FnCodegen::new(
      self,
      func,
      file,
      body,
      lowered.names.as_ref(),
      CodegenMode::FileInit,
      ReturnKind::Void,
      false,
    );

    let mut fallthrough = true;
    for &stmt in &body.root_stmts {
      fallthrough = cg.codegen_stmt(stmt)?;
      if !fallthrough {
        break;
      }
    }
    if fallthrough {
      cg.builder.build_return(None).expect("failed to build return");
    }
    Ok(())
  }

  fn build_ts_function(
    &mut self,
    def: DefId,
    file: FileId,
    body_id: hir_js::BodyId,
    allow_void_return: bool,
  ) -> Result<(), Vec<Diagnostic>> {
    let Some(func) = self.functions.get(&def).copied() else {
      return Ok(());
    };
    if func.get_first_basic_block().is_some() {
      return Ok(());
    }

    let Some(lowered) = self.program.hir_lowered(file) else {
      return Ok(());
    };
    let hir_body = lowered.body(body_id).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0101",
        "failed to access lowered HIR for function body",
        Span::new(file, TextRange::new(0, 0)),
      )]
    })?;
    let Some(function_meta) = hir_body.function.as_ref() else {
      return Err(vec![Diagnostic::error(
        "NJS0102",
        "missing function metadata",
        Span::new(file, hir_body.span),
      )]);
    };

    let mut cg = FnCodegen::new(
      self,
      func,
      file,
      hir_body,
      lowered.names.as_ref(),
      CodegenMode::TsFunction,
      ReturnKind::I32,
      allow_void_return,
    );

    // Parameters.
    for (idx, param) in function_meta.params.iter().enumerate() {
      let binding = cg
        .cg
        .resolver
        .for_file(file)
        .resolve_pat_ident(hir_body, param.pat)
        .ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0122",
            "unsupported parameter pattern",
            Span::new(file, hir_body.span),
          )]
        })?;

      let debug_name = hir_body
        .pats
        .get(param.pat.0 as usize)
        .and_then(|pat| match pat.kind {
          PatKind::Ident(name) => cg.names.resolve(name),
          _ => None,
        })
        .unwrap_or("param");

      let slot = cg.ensure_local_slot(binding, debug_name);
      let value = func
        .get_nth_param(idx as u32)
        .expect("missing param")
        .into_int_value();
      cg.builder.build_store(slot, value).expect("store param");

      if let Some(name) = cg.pat_ident_name(param.pat) {
        cg.env.bind(name, binding);
      }
    }

    match function_meta.body {
      hir_js::FunctionBody::Expr(expr) => {
        let value = cg.codegen_expr(expr)?;
        let ret = if allow_void_return {
          cg.cg.i32_ty.const_zero()
        } else {
          value
        };
        cg.builder
          .build_return(Some(&ret))
          .expect("failed to build return");
      }
      hir_js::FunctionBody::Block(ref stmts) => {
        let mut fallthrough = true;
        for &stmt in stmts {
          fallthrough = cg.codegen_stmt(stmt)?;
          if !fallthrough {
            break;
          }
        }

        if fallthrough {
          if allow_void_return {
            cg.builder
              .build_return(Some(&cg.cg.i32_ty.const_zero()))
              .expect("failed to build implicit return");
          } else {
            return Err(vec![Diagnostic::error(
              "NJS0115",
              "not all control-flow paths return a value in this codegen subset",
              Span::new(file, hir_body.span),
            )]);
          }
        }
      }
    }

    Ok(())
  }

  fn build_c_main(&mut self, ts_main: FunctionValue<'ctx>, init_order: &[FileId]) {
    // Define `main` with no parameters (`int main(void)`), since our generated
    // wrapper does not currently use `argc`/`argv`.
    //
    // This also avoids passing a raw `ptr` argument through a function marked with
    // our GC strategy (`gc \"coreclr\"`), which would violate the GC pointer
    // discipline lint (all pointers in GC function signatures must be
    // `ptr addrspace(1)`).
    let c_main = self.module.add_function("main", self.i32_ty.fn_type(&[], false), None);
    crate::stack_walking::apply_stack_walking_attrs(self.context, c_main);

    let builder = self.context.create_builder();
    let bb = self.context.append_basic_block(c_main, "entry");
    builder.position_at_end(bb);

    for file in init_order {
      if let Some(init) = self.file_inits.get(file).copied() {
        let _ = builder.build_call(init, &[], "init");
      }
    }

    let call = builder
      .build_call(ts_main, &[], "ret")
      .expect("failed to build call");
    let ret_val = call
      .try_as_basic_value()
      .left()
      .map(|v| v.into_int_value())
      .unwrap_or_else(|| self.i32_ty.const_zero());
    builder
      .build_return(Some(&ret_val))
      .expect("failed to build return");
  }

  fn ensure_global_var(&mut self, def: DefId, span: Span) -> Result<GlobalValue<'ctx>, Vec<Diagnostic>> {
    if let Some(existing) = self.globals.get(&def).copied() {
      return Ok(existing);
    }

    let Some(kind) = self.program.def_kind(def) else {
      return Err(vec![Diagnostic::error(
        "NJS0140",
        "failed to resolve definition kind for global binding",
        span,
      )]);
    };

    match kind {
      typecheck_ts::DefKind::Var(_) | typecheck_ts::DefKind::VarDeclarator(_) => {
        let name = crate::llvm_symbol_for_def(self.program, def);
        let global = self.module.add_global(self.i32_ty, None, &name);
        global.set_initializer(&self.i32_ty.const_zero());
        if !self.exported_defs.contains(&def) {
          global.set_linkage(Linkage::Internal);
        }
        self.globals.insert(def, global);
        Ok(global)
      }
      typecheck_ts::DefKind::Import(import) => match import.target {
        typecheck_ts::ImportTarget::File(target_file) => {
          let Some(target) = self
            .program
            .exports_of(target_file)
            .get(import.original.as_str())
            .and_then(|entry| entry.def)
          else {
            return Err(vec![Diagnostic::error(
              "NJS0141",
              format!("failed to resolve imported binding `{}`", import.original),
              span,
            )]);
          };
          self.ensure_global_var(target, span)
        }
        _ => Err(vec![Diagnostic::error(
          "NJS0141",
          "unresolved import in codegen",
          span,
        )]),
      },
      other => Err(vec![Diagnostic::error(
        "NJS0142",
        format!("unsupported global binding kind in codegen: {other:?}"),
        span,
      )]),
    }
  }

  fn resolve_import_def(&self, def: DefId, span: Span) -> Result<DefId, Vec<Diagnostic>> {
    let mut cur = def;
    let mut seen = HashSet::<DefId>::new();
    loop {
      if !seen.insert(cur) {
        return Err(vec![Diagnostic::error(
          "NJS0141",
          "cyclic import resolution in codegen",
          span,
        )]);
      }

      let Some(kind) = self.program.def_kind(cur) else {
        return Err(vec![Diagnostic::error(
          "NJS0140",
          "failed to resolve definition kind for imported binding",
          span,
        )]);
      };

      let typecheck_ts::DefKind::Import(import) = kind else {
        return Ok(cur);
      };

      match import.target {
        typecheck_ts::ImportTarget::File(target_file) => {
          let Some(target) = self
            .program
            .exports_of(target_file)
            .get(import.original.as_str())
            .and_then(|entry| entry.def)
          else {
            return Err(vec![Diagnostic::error(
              "NJS0141",
              format!("failed to resolve imported binding `{}`", import.original),
              span,
            )]);
          };
          cur = target;
        }
        _ => {
          return Err(vec![Diagnostic::error(
            "NJS0141",
            "unresolved import in codegen",
            span,
          )]);
        }
      }
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum ReturnKind {
  I32,
  Void,
}

#[derive(Clone, Copy, Debug)]
enum CodegenMode {
  TsFunction,
  FileInit,
}

#[derive(Clone, Copy)]
struct LoopContext<'ctx> {
  break_bb: BasicBlock<'ctx>,
  continue_bb: BasicBlock<'ctx>,
  label: Option<NameId>,
}

struct LocalEnv {
  scopes: Vec<HashMap<NameId, BindingId>>,
}

impl LocalEnv {
  fn new() -> Self {
    Self {
      scopes: vec![HashMap::new()],
    }
  }

  fn push_scope(&mut self) {
    self.scopes.push(HashMap::new());
  }

  fn pop_scope(&mut self) {
    self.scopes.pop();
  }

  fn bind(&mut self, name: NameId, binding: BindingId) {
    if let Some(scope) = self.scopes.last_mut() {
      scope.insert(name, binding);
    }
  }

  fn resolve(&self, name: NameId) -> Option<BindingId> {
    for scope in self.scopes.iter().rev() {
      if let Some(binding) = scope.get(&name) {
        return Some(*binding);
      }
    }
    None
  }
}

struct FnCodegen<'ctx, 'p, 'a> {
  cg: &'a mut ProgramCodegen<'ctx, 'p>,
  builder: Builder<'ctx>,
  alloca_builder: Builder<'ctx>,
  func: FunctionValue<'ctx>,
  body: &'a hir_js::Body,
  names: &'a hir_js::NameInterner,
  file: FileId,
  locals: HashMap<BindingId, PointerValue<'ctx>>,
  env: LocalEnv,
  loop_stack: Vec<LoopContext<'ctx>>,
  mode: CodegenMode,
  return_kind: ReturnKind,
  allow_void_return: bool,
}

impl<'ctx, 'p, 'a> FnCodegen<'ctx, 'p, 'a> {
  fn new(
    cg: &'a mut ProgramCodegen<'ctx, 'p>,
    func: FunctionValue<'ctx>,
    file: FileId,
    body: &'a hir_js::Body,
    names: &'a hir_js::NameInterner,
    mode: CodegenMode,
    return_kind: ReturnKind,
    allow_void_return: bool,
  ) -> Self {
    let builder = cg.context.create_builder();
    let alloca_builder = cg.context.create_builder();

    let entry_bb = cg.context.append_basic_block(func, "entry");
    builder.position_at_end(entry_bb);

    Self {
      cg,
      builder,
      alloca_builder,
      func,
      body,
      names,
      file,
      locals: HashMap::new(),
      env: LocalEnv::new(),
      loop_stack: Vec::new(),
      mode,
      return_kind,
      allow_void_return,
    }
  }

  fn stmt(&self, stmt: StmtId) -> Result<&hir_js::Stmt, Vec<Diagnostic>> {
    self.body.stmts.get(stmt.0 as usize).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0112",
        "statement id out of bounds",
        Span::new(self.file, self.body.span),
      )]
    })
  }

  fn expr_data(&self, expr: ExprId) -> Result<&hir_js::Expr, Vec<Diagnostic>> {
    self.body.exprs.get(expr.0 as usize).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0103",
        "expression id out of bounds",
        Span::new(self.file, self.body.span),
      )]
    })
  }

  fn pat_ident_name(&self, pat: hir_js::PatId) -> Option<NameId> {
    let pat = self.body.pats.get(pat.0 as usize)?;
    match pat.kind {
      PatKind::Ident(name) => Some(name),
      PatKind::Assign { target, .. } => self.pat_ident_name(target),
      PatKind::AssignTarget(expr) => {
        let expr = self.body.exprs.get(expr.0 as usize)?;
        match expr.kind {
          ExprKind::Ident(name) => Some(name),
          _ => None,
        }
      }
      _ => None,
    }
  }

  fn ensure_local_slot(&mut self, binding: BindingId, debug_name: &str) -> PointerValue<'ctx> {
    if let Some(existing) = self.locals.get(&binding).copied() {
      return existing;
    }

    let entry_bb = self
      .func
      .get_first_basic_block()
      .expect("function must have an entry block");
    if let Some(first) = entry_bb.get_first_instruction() {
      self.alloca_builder.position_before(&first);
    } else {
      self.alloca_builder.position_at_end(entry_bb);
    }

    let slot = self
      .alloca_builder
      .build_alloca(self.cg.i32_ty, debug_name)
      .expect("failed to build alloca");
    self.locals.insert(binding, slot);
    slot
  }

  fn bool_to_i32(&self, v: IntValue<'ctx>) -> IntValue<'ctx> {
    self
      .builder
      .build_int_z_extend(v, self.cg.i32_ty, "bool")
      .expect("failed to zext bool")
  }

  fn is_truthy_i1(&self, v: IntValue<'ctx>) -> IntValue<'ctx> {
    self
      .builder
      .build_int_compare(IntPredicate::NE, v, self.cg.i32_ty.const_zero(), "truthy")
      .expect("failed to build truthy compare")
  }

  fn codegen_stmt(&mut self, stmt_id: StmtId) -> Result<bool, Vec<Diagnostic>> {
    let (kind, span) = {
      let stmt = self.stmt(stmt_id)?;
      (stmt.kind.clone(), Span::new(self.file, stmt.span))
    };

    match kind {
      StmtKind::Empty | StmtKind::Debugger => Ok(true),
      StmtKind::Expr(expr) => {
        if self.codegen_print_stmt(expr)? {
          return Ok(true);
        }
        let _ = self.codegen_expr(expr)?;
        Ok(true)
      }
      StmtKind::Return(Some(_)) if matches!(self.return_kind, ReturnKind::Void) => Err(vec![
        Diagnostic::error("NJS0116", "`return <expr>` is not supported here", span),
      ]),
      StmtKind::Return(Some(expr)) => {
        let value = self.codegen_expr(expr)?;
        let ret = if self.allow_void_return {
          self.cg.i32_ty.const_zero()
        } else {
          value
        };
        self
          .builder
          .build_return(Some(&ret))
          .expect("failed to build return");
        Ok(false)
      }
      StmtKind::Return(None) if matches!(self.return_kind, ReturnKind::Void) => {
        self
          .builder
          .build_return(None)
          .expect("failed to build return");
        Ok(false)
      }
      StmtKind::Return(None) if self.allow_void_return => {
        self
          .builder
          .build_return(Some(&self.cg.i32_ty.const_zero()))
          .expect("failed to build return");
        Ok(false)
      }
      StmtKind::Return(None) => Err(vec![Diagnostic::error(
        "NJS0116",
        "`return` without a value is not supported in this codegen subset",
        span,
      )]),
      StmtKind::Decl(_) => match self.mode {
        CodegenMode::FileInit => Ok(true),
        CodegenMode::TsFunction => Err(vec![Diagnostic::error(
          "NJS0113",
          "nested declarations are not supported in this codegen subset",
          span,
        )]),
      },
      StmtKind::Block(stmts) => {
        self.env.push_scope();
        let mut fallthrough = true;
        for stmt_id in stmts {
          fallthrough = self.codegen_stmt(stmt_id)?;
          if !fallthrough {
            break;
          }
        }
        self.env.pop_scope();
        Ok(fallthrough)
      }
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => self.codegen_if(test, consequent, alternate),
      StmtKind::While { test, body } => self.codegen_while(None, test, body),
      StmtKind::DoWhile { test, body } => self.codegen_do_while(None, test, body),
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => self.codegen_for(None, init.as_ref(), test, update, body, span),
      StmtKind::Var(decl) => {
        self.codegen_var_decl(&decl, span)?;
        Ok(true)
      }
      StmtKind::Break(label) => self.codegen_break(label, span),
      StmtKind::Continue(label) => self.codegen_continue(label, span),
      StmtKind::Labeled { label, body } => self.codegen_labeled(label, body, span),
      StmtKind::Switch { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`switch` statements are not supported yet",
        span,
      )]),
      StmtKind::Try { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`try` statements are not supported yet",
        span,
      )]),
      StmtKind::Throw(_) => Err(vec![Diagnostic::error(
        "NJS0113",
        "`throw` statements are not supported yet",
        span,
      )]),
      StmtKind::ForIn { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`for-in` / `for-of` loops are not supported yet",
        span,
      )]),
      StmtKind::With { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`with` statements are not supported yet",
        span,
      )]),
    }
  }

  fn codegen_print_stmt(&mut self, expr: ExprId) -> Result<bool, Vec<Diagnostic>> {
    let expr = self.expr_data(expr)?;
    let ExprKind::Call(call) = &expr.kind else {
      return Ok(false);
    };
    if call.optional || call.is_new {
      return Ok(false);
    }
    if call.args.len() != 1 {
      return Ok(false);
    }
    let Some(arg) = call.args.first() else {
      return Ok(false);
    };
    if arg.spread {
      return Ok(false);
    }

    if !self.callee_is_global_intrinsic(call.callee, "print") {
      return Ok(false);
    }

    let value = self.codegen_expr(arg.expr)?;
    self.emit_print_i32(value);
    Ok(true)
  }

  fn callee_is_global_intrinsic(&self, expr: ExprId, name: &str) -> bool {
    let Ok(expr) = self.expr_data(expr) else {
      return false;
    };
    let ExprKind::Ident(ident) = expr.kind else {
      return false;
    };
    if self.names.resolve(ident) != Some(name) {
      return false;
    }
    // Don't treat a shadowed local binding as an intrinsic.
    self.env.resolve(ident).is_none()
  }

  fn emit_print_i32(&self, value: IntValue<'ctx>) {
    let printf = declare_printf(self.cg.context, &self.cg.module);
    let fmt = self
      .builder
      .build_global_string_ptr("%d\n", "native_js_print_fmt")
      .expect("failed to create printf format string");
    self
      .builder
      .build_call(
        printf,
        &[fmt.as_pointer_value().into(), value.into()],
        "native_js_print",
      )
      .expect("failed to build printf call");
  }

  fn codegen_break(&mut self, label: Option<NameId>, span: Span) -> Result<bool, Vec<Diagnostic>> {
    let target = if let Some(label) = label {
      self
        .loop_stack
        .iter()
        .rev()
        .find(|ctx| ctx.label == Some(label))
        .copied()
    } else {
      self.loop_stack.last().copied()
    };
    let Some(ctx) = target else {
      return Err(vec![Diagnostic::error(
        if label.is_some() { "NJS0119" } else { "NJS0120" },
        if let Some(label) = label {
          let lbl = self.names.resolve(label).unwrap_or("<label>");
          format!("unknown loop label `{lbl}` for `break`")
        } else {
          "`break` is only supported inside loops".to_string()
        },
        span,
      )]);
    };
    self
      .builder
      .build_unconditional_branch(ctx.break_bb)
      .expect("failed to build break branch");
    Ok(false)
  }

  fn codegen_continue(&mut self, label: Option<NameId>, span: Span) -> Result<bool, Vec<Diagnostic>> {
    let target = if let Some(label) = label {
      self
        .loop_stack
        .iter()
        .rev()
        .find(|ctx| ctx.label == Some(label))
        .copied()
    } else {
      self.loop_stack.last().copied()
    };
    let Some(ctx) = target else {
      return Err(vec![Diagnostic::error(
        if label.is_some() { "NJS0121" } else { "NJS0122" },
        if let Some(label) = label {
          let lbl = self.names.resolve(label).unwrap_or("<label>");
          format!("unknown loop label `{lbl}` for `continue`")
        } else {
          "`continue` is only supported inside loops".to_string()
        },
        span,
      )]);
    };
    self
      .builder
      .build_unconditional_branch(ctx.continue_bb)
      .expect("failed to build continue branch");
    Ok(false)
  }

  fn codegen_labeled(
    &mut self,
    label: NameId,
    body: StmtId,
    span: Span,
  ) -> Result<bool, Vec<Diagnostic>> {
    let kind = self.stmt(body)?.kind.clone();
    match kind {
      StmtKind::While { test, body } => self.codegen_while(Some(label), test, body),
      StmtKind::DoWhile { test, body } => self.codegen_do_while(Some(label), test, body),
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => self.codegen_for(Some(label), init.as_ref(), test, update, body, span),
      _ => Err(vec![Diagnostic::error(
        "NJS0124",
        "only labeled loops are supported in native-js codegen",
        span,
      )]),
    }
  }

  fn codegen_if(
    &mut self,
    test: ExprId,
    consequent: StmtId,
    alternate: Option<StmtId>,
  ) -> Result<bool, Vec<Diagnostic>> {
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);

    let then_bb = self.cg.context.append_basic_block(self.func, "if.then");

    // If there is no alternate, the false branch falls through directly.
    if alternate.is_none() {
      let cont_bb = self.cg.context.append_basic_block(self.func, "if.end");
      self
        .builder
        .build_conditional_branch(cond_i1, then_bb, cont_bb)
        .expect("failed to build conditional branch");

      self.builder.position_at_end(then_bb);
      let then_fallthrough = self.codegen_stmt(consequent)?;
      if then_fallthrough {
        self
          .builder
          .build_unconditional_branch(cont_bb)
          .expect("failed to build branch");
      }

      self.builder.position_at_end(cont_bb);
      return Ok(true);
    }

    let else_bb = self.cg.context.append_basic_block(self.func, "if.else");
    self
      .builder
      .build_conditional_branch(cond_i1, then_bb, else_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(then_bb);
    let then_fallthrough = self.codegen_stmt(consequent)?;

    let mut cont_bb = None;
    if then_fallthrough {
      let bb = self.cg.context.append_basic_block(self.func, "if.end");
      self
        .builder
        .build_unconditional_branch(bb)
        .expect("failed to build branch");
      cont_bb = Some(bb);
    }

    self.builder.position_at_end(else_bb);
    let else_fallthrough = self.codegen_stmt(alternate.expect("checked above"))?;
    if else_fallthrough {
      let bb = cont_bb.unwrap_or_else(|| self.cg.context.append_basic_block(self.func, "if.end"));
      self
        .builder
        .build_unconditional_branch(bb)
        .expect("failed to build branch");
      cont_bb = Some(bb);
    }

    if let Some(cont) = cont_bb {
      self.builder.position_at_end(cont);
      Ok(true)
    } else {
      Ok(false)
    }
  }

  fn codegen_while(
    &mut self,
    label: Option<NameId>,
    test: ExprId,
    body: StmtId,
  ) -> Result<bool, Vec<Diagnostic>> {
    let cond_bb = self.cg.context.append_basic_block(self.func, "while.cond");
    let body_bb = self.cg.context.append_basic_block(self.func, "while.body");
    let end_bb = self.cg.context.append_basic_block(self.func, "while.end");

    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build branch");

    self.builder.position_at_end(cond_bb);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(body_bb);
    self.loop_stack.push(LoopContext {
      break_bb: end_bb,
      continue_bb: cond_bb,
      label,
    });
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");
    }
    self.loop_stack.pop();

    self.builder.position_at_end(end_bb);
    Ok(true)
  }

  fn codegen_do_while(
    &mut self,
    label: Option<NameId>,
    test: ExprId,
    body: StmtId,
  ) -> Result<bool, Vec<Diagnostic>> {
    let body_bb = self.cg.context.append_basic_block(self.func, "do.body");
    let cond_bb = self.cg.context.append_basic_block(self.func, "do.cond");
    let end_bb = self.cg.context.append_basic_block(self.func, "do.end");

    self
      .builder
      .build_unconditional_branch(body_bb)
      .expect("failed to build branch");

    self.loop_stack.push(LoopContext {
      break_bb: end_bb,
      continue_bb: cond_bb,
      label,
    });

    self.builder.position_at_end(body_bb);
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");
    }

    self.builder.position_at_end(cond_bb);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.loop_stack.pop();

    self.builder.position_at_end(end_bb);
    Ok(true)
  }

  fn codegen_for(
    &mut self,
    label: Option<NameId>,
    init: Option<&ForInit>,
    test: Option<ExprId>,
    update: Option<ExprId>,
    body: StmtId,
    span: Span,
  ) -> Result<bool, Vec<Diagnostic>> {
    // `for (let/const ...)` introduces a lexical scope that does *not* leak
    // outside the loop. Without this, shadowing an outer `let` via a loop
    // initializer would incorrectly override the outer binding for the remainder
    // of the function.
    let needs_loop_scope = matches!(
      init,
      Some(ForInit::Var(decl)) if matches!(decl.kind, VarDeclKind::Let | VarDeclKind::Const)
    );
    if needs_loop_scope {
      self.env.push_scope();
    }

    let result = (|| -> Result<bool, Vec<Diagnostic>> {
      if let Some(init) = init {
        match init {
          ForInit::Expr(expr) => {
            let _ = self.codegen_expr(*expr)?;
          }
          ForInit::Var(decl) => {
            self.codegen_var_decl(decl, span)?;
          }
        }
      }

      let cond_bb = self.cg.context.append_basic_block(self.func, "for.cond");
      let body_bb = self.cg.context.append_basic_block(self.func, "for.body");
      let update_bb = self.cg.context.append_basic_block(self.func, "for.update");
      let end_bb = self.cg.context.append_basic_block(self.func, "for.end");

      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");

      self.builder.position_at_end(cond_bb);
      let cond_i1 = if let Some(test) = test {
        let v = self.codegen_expr(test)?;
        self.is_truthy_i1(v)
      } else {
        self.cg.bool_ty.const_int(1, false)
      };
      self
        .builder
        .build_conditional_branch(cond_i1, body_bb, end_bb)
        .expect("failed to build conditional branch");

      self.builder.position_at_end(body_bb);
      self.loop_stack.push(LoopContext {
        break_bb: end_bb,
        continue_bb: update_bb,
        label,
      });
      let body_fallthrough = self.codegen_stmt(body)?;
      if body_fallthrough {
        self
          .builder
          .build_unconditional_branch(update_bb)
          .expect("failed to build branch");
      }
      self.loop_stack.pop();

      self.builder.position_at_end(update_bb);
      if let Some(update) = update {
        let _ = self.codegen_expr(update)?;
      }
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");

      self.builder.position_at_end(end_bb);
      Ok(true)
    })();

    if needs_loop_scope {
      self.env.pop_scope();
    }

    result
  }

  fn codegen_var_decl(&mut self, decl: &VarDecl, span: Span) -> Result<(), Vec<Diagnostic>> {
    match decl.kind {
      VarDeclKind::Var | VarDeclKind::Let | VarDeclKind::Const => {}
      _ => {
        return Err(vec![Diagnostic::error(
          "NJS0113",
          "unsupported variable declaration kind in native-js codegen",
          span,
        )]);
      }
    }

    for declarator in decl.declarators.iter() {
      let binding = self
        .cg
        .resolver
        .for_file(self.file)
        .resolve_pat_ident(self.body, declarator.pat)
        .ok_or_else(|| {
          let pat_span = self
            .body
            .pats
            .get(declarator.pat.0 as usize)
            .map(|pat| pat.span)
            .unwrap_or(span.range);
          vec![Diagnostic::error(
            "NJS0122",
            "unsupported variable binding pattern",
            Span::new(self.file, pat_span),
          )]
        })?;

      let debug_name = self
        .body
        .pats
        .get(declarator.pat.0 as usize)
        .and_then(|pat| match pat.kind {
          PatKind::Ident(name) => self.names.resolve(name),
          _ => None,
        })
        .unwrap_or("local");

      let Some(init) = declarator.init else {
        return Err(vec![Diagnostic::error(
          "NJS0118",
          "variable declarations must have an initializer in native-js codegen",
          span,
        )]);
      };

      let value = self.codegen_expr(init)?;

      match binding {
        BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
          let global = self.cg.ensure_global_var(def, span)?;
          self
            .builder
            .build_store(global.as_pointer_value(), value)
            .expect("failed to build store");
        }
        _ => {
          let slot = self.ensure_local_slot(binding, debug_name);
          self
            .builder
            .build_store(slot, value)
            .expect("failed to build store");

          if let Some(name) = self.pat_ident_name(declarator.pat) {
            self.env.bind(name, binding);
          }
        }
      }
    }
    Ok(())
  }

  fn ptr_for_binding(&mut self, binding: BindingId, span: Span) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    if let Some(ptr) = self.locals.get(&binding).copied() {
      return Ok(ptr);
    }
    match binding {
      BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
        let global = self.cg.ensure_global_var(def, span)?;
        Ok(global.as_pointer_value())
      }
      _ => Err(vec![Diagnostic::error(
        "NJS0114",
        "use of unknown/unbound identifier in native-js codegen",
        span,
      )]),
    }
  }

  fn codegen_expr(&mut self, expr: ExprId) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let (kind, span) = {
      let expr_data = self.expr_data(expr)?;
      (
        expr_data.kind.clone(),
        Span::new(self.file, expr_data.span),
      )
    };

    match kind {
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Satisfies { expr, .. } => self.codegen_expr(expr),
      ExprKind::Literal(Literal::Number(raw)) => parse_i32_const(self.cg.i32_ty, &raw).ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0104",
          format!("unsupported numeric literal `{raw}` (expected 32-bit integer)"),
          span,
        )]
      }),
      ExprKind::Literal(Literal::Boolean(b)) => Ok(self.cg.i32_ty.const_int(u64::from(b), false)),
      ExprKind::Unary { op, expr } => {
        let inner = self.codegen_expr(expr)?;
        match op {
          UnaryOp::Plus => Ok(inner),
          UnaryOp::Minus => Ok(
            self
              .builder
              .build_int_neg(inner, "neg")
              .expect("failed to build negation"),
          ),
          UnaryOp::Not => {
            let is_false = self
              .builder
              .build_int_compare(IntPredicate::EQ, inner, self.cg.i32_ty.const_zero(), "not")
              .expect("failed to build compare");
            Ok(self.bool_to_i32(is_false))
          }
          UnaryOp::BitNot => Ok(self
            .builder
            .build_not(inner, "bitnot")
            .expect("failed to build bitnot")),
          _ => Err(vec![Diagnostic::error(
            "NJS0105",
            format!("unsupported unary operator `{op:?}`"),
            span,
          )]),
        }
      }
      ExprKind::Binary { op, left, right } => {
        let lhs = self.codegen_expr(left)?;
        let rhs = self.codegen_expr(right)?;
        let v = match op {
          BinaryOp::Add => self
            .builder
            .build_int_add(lhs, rhs, "add")
            .expect("failed to build add"),
          BinaryOp::Subtract => self
            .builder
            .build_int_sub(lhs, rhs, "sub")
            .expect("failed to build sub"),
          BinaryOp::Multiply => self
            .builder
            .build_int_mul(lhs, rhs, "mul")
            .expect("failed to build mul"),
          BinaryOp::Divide => self
            .builder
            .build_int_signed_div(lhs, rhs, "div")
            .expect("failed to build div"),
          BinaryOp::Remainder => self
            .builder
            .build_int_signed_rem(lhs, rhs, "rem")
            .expect("failed to build rem"),
          BinaryOp::BitAnd => self.builder.build_and(lhs, rhs, "and").expect("failed to build and"),
          BinaryOp::BitOr => self.builder.build_or(lhs, rhs, "or").expect("failed to build or"),
          BinaryOp::BitXor => self.builder.build_xor(lhs, rhs, "xor").expect("failed to build xor"),
          BinaryOp::ShiftLeft => self
            .builder
            .build_left_shift(lhs, rhs, "shl")
            .expect("failed to build shl"),
          BinaryOp::ShiftRight => self
            .builder
            .build_right_shift(lhs, rhs, true, "shr")
            .expect("failed to build shr"),
          BinaryOp::LessThan
          | BinaryOp::LessEqual
          | BinaryOp::GreaterThan
          | BinaryOp::GreaterEqual
          | BinaryOp::Equality
          | BinaryOp::Inequality
          | BinaryOp::StrictEquality
          | BinaryOp::StrictInequality => {
            let pred = match op {
              BinaryOp::LessThan => IntPredicate::SLT,
              BinaryOp::LessEqual => IntPredicate::SLE,
              BinaryOp::GreaterThan => IntPredicate::SGT,
              BinaryOp::GreaterEqual => IntPredicate::SGE,
              BinaryOp::Equality | BinaryOp::StrictEquality => IntPredicate::EQ,
              BinaryOp::Inequality | BinaryOp::StrictInequality => IntPredicate::NE,
              _ => unreachable!(),
            };
            let cmp = self
              .builder
              .build_int_compare(pred, lhs, rhs, "cmp")
              .expect("failed to build compare");
            self.bool_to_i32(cmp)
          }
          _ => {
            return Err(vec![Diagnostic::error(
              "NJS0106",
              format!("unsupported binary operator `{op:?}`"),
              span,
            )]);
          }
        };
        Ok(v)
      }
      ExprKind::Ident(name) => {
        let binding = if let Some(binding) = self.env.resolve(name) {
          binding
        } else {
          self
            .cg
            .resolver
            .for_file(self.file)
            .resolve_expr_ident(self.body, expr)
            .ok_or_else(|| vec![Diagnostic::error("NJS0130", "failed to resolve identifier", span)])?
        };

        if let Some(ptr) = self.locals.get(&binding).copied() {
          return Ok(
            self
              .builder
              .build_load(self.cg.i32_ty, ptr, "load")
              .expect("failed to build load")
              .into_int_value(),
          );
        }

        match binding {
          BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
            let global = self.cg.ensure_global_var(def, span)?;
            Ok(
              self
                .builder
                .build_load(self.cg.i32_ty, global.as_pointer_value(), "global.load")
                .expect("failed to build load")
                .into_int_value(),
            )
          }
          _ => {
            let label = self.names.resolve(name).unwrap_or("<unknown>");
            Err(vec![Diagnostic::error(
              "NJS0114",
              format!("unknown identifier `{label}` in native-js codegen"),
              span,
            )])
          }
        }
      }
      ExprKind::Assignment { op, target, value } => {
        let binding = if let Some(name) = self.pat_ident_name(target) {
          if let Some(binding) = self.env.resolve(name) {
            binding
          } else {
            self
              .cg
              .resolver
              .for_file(self.file)
              .resolve_pat_ident(self.body, target)
              .ok_or_else(|| {
                let pat_span = self
                  .body
                  .pats
                  .get(target.0 as usize)
                  .map(|pat| pat.span)
                  .unwrap_or(span.range);
                vec![Diagnostic::error(
                  "NJS0132",
                  "unsupported assignment target",
                  Span::new(self.file, pat_span),
                )]
              })?
          }
        } else {
          self
            .cg
            .resolver
            .for_file(self.file)
            .resolve_pat_ident(self.body, target)
            .ok_or_else(|| {
              let pat_span = self
                .body
                .pats
                .get(target.0 as usize)
                .map(|pat| pat.span)
                .unwrap_or(span.range);
              vec![Diagnostic::error(
                "NJS0132",
                "unsupported assignment target",
                Span::new(self.file, pat_span),
              )]
            })?
        };

        let ptr = self.ptr_for_binding(binding, span)?;
        let rhs = self.codegen_expr(value)?;
        self.codegen_assignment_to_ptr(ptr, span, &op, rhs)
      }
      ExprKind::Update { op, expr, prefix } => {
        let inner = self.expr_data(expr)?;
        let ExprKind::Ident(name) = inner.kind else {
          return Err(vec![Diagnostic::error(
            "NJS0107",
            "unsupported update target (expected identifier)",
            Span::new(self.file, inner.span),
          )]);
        };

        let binding = if let Some(binding) = self.env.resolve(name) {
          binding
        } else {
          self
            .cg
            .resolver
            .for_file(self.file)
            .resolve_expr_ident(self.body, expr)
            .ok_or_else(|| {
              vec![Diagnostic::error(
                "NJS0130",
                "failed to resolve update target",
                Span::new(self.file, inner.span),
              )]
            })?
        };
        let ptr = self.ptr_for_binding(binding, Span::new(self.file, inner.span))?;

        let old = self
          .builder
          .build_load(self.cg.i32_ty, ptr, "load")
          .expect("failed to build load")
          .into_int_value();
        let one = self.cg.i32_ty.const_int(1, false);
        let new = match op {
          UpdateOp::Increment => self
            .builder
            .build_int_add(old, one, "inc")
            .expect("failed to build inc"),
          UpdateOp::Decrement => self
            .builder
            .build_int_sub(old, one, "dec")
            .expect("failed to build dec"),
        };
        self
          .builder
          .build_store(ptr, new)
          .expect("failed to build store");
        Ok(if prefix { new } else { old })
      }
      ExprKind::Call(call) => {
        if call.optional || call.is_new {
          return Err(vec![Diagnostic::error(
            "NJS0144",
            "unsupported call syntax in codegen",
            span,
          )]);
        }

        let callee_expr = self
          .body
          .exprs
          .get(call.callee.0 as usize)
          .ok_or_else(|| vec![Diagnostic::error("NJS0103", "callee id out of bounds", span)])?;
        if !matches!(callee_expr.kind, ExprKind::Ident(_)) {
          return Err(vec![Diagnostic::error(
            "NJS0144",
            "only direct identifier calls are supported in this codegen subset",
            span,
          )]);
        }

        let binding = self
          .cg
          .resolver
          .for_file(self.file)
          .resolve_expr_ident(self.body, call.callee)
          .ok_or_else(|| vec![Diagnostic::error("NJS0130", "failed to resolve call callee", span)])?;
        let BindingId::Def(def) = binding else {
          return Err(vec![Diagnostic::error(
            "NJS0144",
            "callee must resolve to a global function definition",
            span,
          )]);
        };

        let resolved = self.cg.resolve_import_def(def, span)?;
        let Some(target) = self.cg.functions.get(&resolved).copied() else {
          return Err(vec![Diagnostic::error(
            "NJS0145",
            "call to unknown function in codegen",
            span,
          )]);
        };
        if def != resolved {
          self.cg.functions.entry(def).or_insert(target);
        }

        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
          if arg.spread {
            return Err(vec![Diagnostic::error(
              "NJS0144",
              "spread arguments are not supported in this codegen subset",
              span,
            )]);
          }
          let value = self.codegen_expr(arg.expr)?;
          args.push(value.into());
        }

        let call = self
          .builder
          .build_call(target, &args, "call")
          .expect("failed to build call");
        let ret = call
          .try_as_basic_value()
          .left()
          .ok_or_else(|| vec![Diagnostic::error("NJS0145", "void call not supported", span)])?
          .into_int_value();
        Ok(ret)
      }
      _ => Err(vec![Diagnostic::error(
        "NJS0107",
        "unsupported expression in native-js codegen",
        span,
      )]),
    }
  }

  fn codegen_assignment_to_ptr(
    &self,
    ptr: PointerValue<'ctx>,
    span: Span,
    op: &AssignOp,
    rhs: IntValue<'ctx>,
  ) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let out = match op {
      AssignOp::Assign => rhs,
      AssignOp::AddAssign
      | AssignOp::SubAssign
      | AssignOp::MulAssign
      | AssignOp::DivAssign
      | AssignOp::RemAssign => {
        let cur = self
          .builder
          .build_load(self.cg.i32_ty, ptr, "load")
          .expect("failed to build load")
          .into_int_value();
        match op {
          AssignOp::AddAssign => self
            .builder
            .build_int_add(cur, rhs, "addassign")
            .expect("failed to build add"),
          AssignOp::SubAssign => self
            .builder
            .build_int_sub(cur, rhs, "subassign")
            .expect("failed to build sub"),
          AssignOp::MulAssign => self
            .builder
            .build_int_mul(cur, rhs, "mulassign")
            .expect("failed to build mul"),
          AssignOp::DivAssign => self
            .builder
            .build_int_signed_div(cur, rhs, "divassign")
            .expect("failed to build div"),
          AssignOp::RemAssign => self
            .builder
            .build_int_signed_rem(cur, rhs, "remassign")
            .expect("failed to build rem"),
          _ => unreachable!(),
        }
      }
      _ => {
        return Err(vec![Diagnostic::error(
          "NJS0134",
          format!("unsupported assignment operator `{op:?}`"),
          span,
        )]);
      }
    };

    self
      .builder
      .build_store(ptr, out)
      .expect("failed to build store");
    Ok(out)
  }
}

fn file_import_deps(program: &Program, lowered: &hir_js::LowerResult) -> Vec<FileId> {
  // Keep module dependencies in the same order as the source-level `import`
  // statements. This matches JS module evaluation semantics and provides
  // deterministic initialization order for sibling imports.
  let from = lowered.hir.file;
  let mut deps = Vec::new();
  let mut seen = HashSet::<FileId>::new();
  for import in &lowered.hir.imports {
    let ImportKind::Es(es) = &import.kind else {
      continue;
    };
    if es.is_type_only {
      continue;
    }
    let Some(dep) = program.resolve_module(from, es.specifier.value.as_str()) else {
      continue;
    };
    if seen.insert(dep) {
      deps.push(dep);
    }
  }
  deps
}

fn topo_visit(
  file: FileId,
  deps: &HashMap<FileId, Vec<FileId>>,
  visited: &mut HashSet<FileId>,
  visiting: &mut HashSet<FileId>,
  out: &mut Vec<FileId>,
) {
  if visited.contains(&file) {
    return;
  }
  if !visiting.insert(file) {
    // Cycle: best-effort, keep deterministic order.
    return;
  }
  if let Some(children) = deps.get(&file) {
    for dep in children {
      topo_visit(*dep, deps, visited, visiting, out);
    }
  }
  visiting.remove(&file);
  visited.insert(file);
  out.push(file);
}

fn is_toplevel_def(program: &Program, def: DefId) -> bool {
  let Some(lowered) = program.hir_lowered(def.file()) else {
    return false;
  };
  let mut cur = def;
  loop {
    let Some(data) = lowered.def(cur) else {
      return false;
    };
    // `hir-js` scopes many local bindings under their owning function/method
    // definition. Top-level module bindings (including `let`/`const` globals and
    // imports) have no function-like ancestor.
    match data.path.kind {
      hir_js::DefKind::Function
      | hir_js::DefKind::Method
      | hir_js::DefKind::Constructor
      | hir_js::DefKind::Getter
      | hir_js::DefKind::Setter => return false,
      _ => {}
    }
    let Some(parent) = data.parent else {
      break;
    };
    cur = parent;
  }
  true
}

fn declare_printf<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("printf") {
    return existing;
  }
  let i32_ty = context.i32_type();
  let ptr_ty = context.i8_type().ptr_type(AddressSpace::default());
  module.add_function("printf", i32_ty.fn_type(&[ptr_ty.into()], true), None)
}

fn parse_i32_const<'ctx>(i32_ty: IntType<'ctx>, raw: &str) -> Option<IntValue<'ctx>> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let normalized: String = raw.chars().filter(|c| *c != '_').collect();
  let (radix, digits) = if let Some(rest) = normalized.strip_prefix("0x") {
    (16, rest)
  } else if let Some(rest) = normalized.strip_prefix("0X") {
    (16, rest)
  } else if let Some(rest) = normalized.strip_prefix("0b") {
    (2, rest)
  } else if let Some(rest) = normalized.strip_prefix("0B") {
    (2, rest)
  } else if let Some(rest) = normalized.strip_prefix("0o") {
    (8, rest)
  } else if let Some(rest) = normalized.strip_prefix("0O") {
    (8, rest)
  } else {
    if normalized.contains('.') || normalized.contains('e') || normalized.contains('E') {
      return None;
    }
    (10, normalized.as_str())
  };

  let value = i64::from_str_radix(digits, radix).ok()?;
  let value = i32::try_from(value).ok()?;
  Some(i32_ty.const_int(value as u64, true))
}

mod builtins;
pub mod safepoint;
pub(crate) mod llvm;

use crate::CompileOptions;
use parse_js::ast::node::Node;
use parse_js::ast::stx::TopLevel;

#[derive(thiserror::Error, Debug)]
pub enum CodegenError {
  #[error("unsupported statement")]
  UnsupportedStmt,

  #[error("unsupported expression")]
  UnsupportedExpr,

  #[error("unsupported operator: {0:?}")]
  UnsupportedOperator(parse_js::operator::OperatorName),

  #[error("builtins disabled")]
  BuiltinsDisabled,

  #[error("type error: {0}")]
  TypeError(String),
}

pub fn emit_llvm_module(ast: &Node<TopLevel>, opts: CompileOptions) -> Result<String, CodegenError> {
  llvm::emit_llvm_module(ast, opts)
}
