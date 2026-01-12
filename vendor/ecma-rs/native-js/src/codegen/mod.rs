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
//! The HIR backend emits stable codes for codegen failures:
//! - `NJS0011`: unsupported type for native codegen (e.g. unsupported function ABI/signature)
//! - `NJS01xx`: HIR-lowering/codegen failures in the current backend subset, including:
//!
//! - `NJS0100`: failed to access lowered HIR for entry file / entry file must not be a declaration file
//! - `NJS0101`: failed to access lowered HIR for a function body / failed to locate `main` for codegen
//! - `NJS0102`: missing function metadata
//! - `NJS0103`: expression id out of bounds
//! - `NJS0104`: invalid numeric literal
//! - `NJS0105`: unsupported unary operator
//! - `NJS0106`: unsupported binary operator
//! - `NJS0107`: unsupported expression in the current codegen subset
//! - `NJS0112`: statement id out of bounds
//! - `NJS0113`: unsupported statement / variable declaration kind in the current codegen subset
//! - `NJS0114`: use of unknown/unbound identifier in the current codegen subset
//! - `NJS0115`: not all control-flow paths return a value in the current codegen subset
//! - `NJS0116`: unsupported `return` statement form in this context (`return;` is only allowed when the function returns `void`/`undefined`/`never`)
//! - `NJS0118`: variable declarations must have an initializer
//! - `NJS0119`: unknown loop label for `break`
//! - `NJS0120`: `break` is only supported inside loops
//! - `NJS0121`: unknown loop label for `continue`
//! - `NJS0122`: `continue` is only supported inside loops (also used for unsupported binding patterns)
//! - `NJS0123`: failed to resolve call signature for exported `main`
//! - `NJS0124`: labels are only supported on loops in native-js codegen
//! - `NJS0130`: failed to resolve identifier/callee during codegen
//! - `NJS0132`: unsupported assignment target
//! - `NJS0134`: unsupported assignment operator
//! - `NJS0140`: failed to resolve definition kind for a global/import binding
//! - `NJS0141`: unresolved import binding (or cyclic import resolution)
//! - `NJS0142`: unsupported global binding kind in codegen
//! - `NJS0144`: unsupported call syntax in codegen subset
//! - `NJS0145`: call to unknown function (or void call not supported)
//! - `NJS0146`: cyclic module dependency detected in runtime module graph
//!
//! Entrypoint-related errors are emitted by [`crate::strict::entrypoint`]
//! (`NJS0108..NJS0111`).
use crate::builtins::NativeJsIntrinsic;
use crate::resolve::BindingId;
use crate::runtime_abi::{RuntimeAbi, RuntimeFn};
use crate::strict::Entrypoint;
use crate::codes;
use crate::Resolver;
mod debuginfo;
use diagnostics::{Diagnostic, Label, Span, TextRange};
use hir_js::{
  AssignOp, BinaryOp, ExprId, ExprKind, FileKind, ForInit, ImportKind, Literal, NameId, PatKind,
  StmtId, StmtKind, UnaryOp, UpdateOp, VarDecl, VarDeclKind,
};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicType, BasicTypeEnum, FloatType, IntType};
use inkwell::values::{
  BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, GlobalValue, IntValue,
  PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};
use parse_js::num::JsNumber;
use std::collections::{HashMap, HashSet, VecDeque};
use typecheck_ts::{DefId, FileId, Program, TypeKindSummary};

pub struct CodegenOptions {
  pub module_name: String,
  /// Whether to emit DWARF debug info metadata (`llvm.dbg.*` intrinsics, `!DI*` nodes, etc).
  ///
  /// This is primarily used by the typechecked/native pipeline (`native-js-cli`, `native-js`).
  pub debug: bool,
  /// The optimization level the final artifact will be built with.
  ///
  /// This is used as a heuristic for emitting "optimized debug info" (variable tracking via
  /// `llvm.dbg.value` instead of only `llvm.dbg.declare` attached to allocas).
  pub opt_level: crate::OptLevel,
}

impl Default for CodegenOptions {
  fn default() -> Self {
    Self {
      module_name: "native_js".to_string(),
      debug: false,
      opt_level: crate::OptLevel::O0,
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
  let mut cg = ProgramCodegen::new(context, program, entry_file, &options);
  cg.compile(entry_file, entrypoint)?;
  Ok(cg.finish())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TsAbiKind {
  Number,
  Boolean,
  Void,
}

impl TsAbiKind {
  fn from_value_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never => Some(Self::Void),
      _ => None,
    }
  }

  fn from_param_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      _ => None,
    }
  }

  fn from_return_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never => Some(Self::Void),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum NativeValue<'ctx> {
  Number(FloatValue<'ctx>),
  Boolean(IntValue<'ctx>),
  Void,
}

impl<'ctx> NativeValue<'ctx> {
  fn kind(self) -> TsAbiKind {
    match self {
      NativeValue::Number(_) => TsAbiKind::Number,
      NativeValue::Boolean(_) => TsAbiKind::Boolean,
      NativeValue::Void => TsAbiKind::Void,
    }
  }

  fn as_basic_value(self) -> Option<BasicValueEnum<'ctx>> {
    match self {
      NativeValue::Number(v) => Some(v.into()),
      NativeValue::Boolean(v) => Some(v.into()),
      NativeValue::Void => None,
    }
  }

  fn into_basic_metadata(self) -> Option<BasicMetadataValueEnum<'ctx>> {
    self.as_basic_value().map(Into::into)
  }
}

#[derive(Clone, Debug)]
struct TsFunctionSigKind {
  #[allow(dead_code)]
  params: Vec<TsAbiKind>,
  ret: TsAbiKind,
}

#[derive(Clone, Copy)]
struct GlobalSlot<'ctx> {
  global: GlobalValue<'ctx>,
  kind: TsAbiKind,
}

#[derive(Clone, Copy)]
struct LocalSlot<'ctx> {
  ptr: PointerValue<'ctx>,
  kind: TsAbiKind,
}

fn ts_function_sig_kind(
  program: &Program,
  def: DefId,
  file: FileId,
  expected_param_count: usize,
  is_entrypoint: bool,
) -> Result<TsFunctionSigKind, Vec<Diagnostic>> {
  let span = program
    .span_of_def(def)
    .unwrap_or_else(|| Span::new(file, TextRange::new(0, 0)));

  let func_ty = program.type_of_def_interned(def);
  let sigs = program.call_signatures(func_ty);
  if sigs.is_empty() {
    if is_entrypoint {
      return Err(vec![codes::HIR_CODEGEN_MAIN_SIGNATURE_MISSING.error(
        "failed to resolve call signature for exported `main`",
        Span::new(file, TextRange::new(0, 0)),
      )]);
    }

    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "failed to resolve call signature for function",
      span,
    )]);
  }

  if sigs.len() != 1 {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "only single-signature functions are supported by native-js right now",
      span,
    )]);
  }

  let sig = &sigs[0].signature;
  if sig.this_param.is_some() {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "`this` parameters are not supported by native-js",
      span,
    )]);
  }
  if !sig.type_params.is_empty() {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "generic functions are not supported by native-js",
      span,
    )]);
  }

  if sig.params.len() != expected_param_count {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "native-js: signature/parameter count mismatch",
      span,
    )]);
  }

  let mut params = Vec::with_capacity(sig.params.len());
  for (idx, param) in sig.params.iter().enumerate() {
    if param.optional || param.rest {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!("optional/rest parameters are not supported by native-js yet (param #{idx})"),
        span,
      )]);
    }

    let kind = program.type_kind(param.ty);
    let Some(kind) = TsAbiKind::from_param_type_kind(&kind) else {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported parameter type for native-js ABI (expected number|boolean): {}",
          program.display_type(param.ty)
        ),
        span,
      )]);
    };
    params.push(kind);
  }

  let ret_kind = program.type_kind(sig.ret);
  let Some(ret) = TsAbiKind::from_return_type_kind(&ret_kind) else {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      format!(
        "unsupported return type for native-js ABI (expected number|boolean|void): {}",
        program.display_type(sig.ret)
      ),
      span,
    )]);
  };

  Ok(TsFunctionSigKind { params, ret })
}

struct ProgramCodegen<'ctx, 'p> {
  context: &'ctx Context,
  module: Module<'ctx>,
  f64_ty: FloatType<'ctx>,
  i32_ty: IntType<'ctx>,
  i64_ty: IntType<'ctx>,
  i1_ty: IntType<'ctx>,
  program: &'p Program,
  resolver: Resolver<'p>,
  exported_defs: HashSet<DefId>,
  globals: HashMap<DefId, GlobalSlot<'ctx>>,
  functions: HashMap<DefId, FunctionValue<'ctx>>,
  function_sigs: HashMap<DefId, TsFunctionSigKind>,
  file_inits: HashMap<FileId, FunctionValue<'ctx>>,
  debug: Option<debuginfo::CodegenDebug<'ctx>>,
}

impl<'ctx, 'p> ProgramCodegen<'ctx, 'p> {
  fn new(context: &'ctx Context, program: &'p Program, entry_file: FileId, options: &CodegenOptions) -> Self {
    let module = context.create_module(&options.module_name);
    let debug = options
      .debug
      .then(|| debuginfo::CodegenDebug::new(&module, program, entry_file, options.opt_level));
    Self {
      context,
      module,
      f64_ty: context.f64_type(),
      i32_ty: context.i32_type(),
      i64_ty: context.i64_type(),
      i1_ty: context.bool_type(),
      program,
      resolver: Resolver::new(program),
      exported_defs: HashSet::new(),
      globals: HashMap::new(),
      functions: HashMap::new(),
      function_sigs: HashMap::new(),
      file_inits: HashMap::new(),
      debug,
    }
  }

  fn finish(self) -> Module<'ctx> {
    if let Some(debug) = self.debug.as_ref() {
      debug.finalize();
    }
    self.module
  }

  fn compile(&mut self, entry_file: FileId, entrypoint: Entrypoint) -> Result<(), Vec<Diagnostic>> {
    let Some(lowered) = self.program.hir_lowered(entry_file) else {
      return Err(vec![codes::HIR_CODEGEN_MISSING_ENTRY_HIR.error(
        "failed to access lowered HIR for entry file",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]);
    };
    if matches!(lowered.hir.file_kind, FileKind::Dts) {
      return Err(vec![codes::HIR_CODEGEN_MISSING_ENTRY_HIR.error(
        "entry file must not be a declaration file",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]);
    }

    let main_def = entrypoint.main_def;
    let main_sig = ts_function_sig_kind(self.program, main_def, entry_file, 0, true)?;

    let files = self.runtime_files(entry_file);
    self.collect_exported_defs(&files);
    let init_order = self.init_order(entry_file, &files)?;

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

        let sig = if def.id == main_def {
          main_sig.clone()
        } else {
          ts_function_sig_kind(self.program, def.id, *file, function.params.len(), false)?
        };
        self.ensure_ts_function(def.id, sig);
        function_bodies.push((def.id, *file, body_id));
      }
    }

    let Some(main_fn) = self.functions.get(&main_def).copied() else {
      return Err(vec![codes::HIR_CODEGEN_MISSING_FUNCTION_HIR.error(
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
      self.build_ts_function(def, file, body_id)?;
    }

    // Build C entrypoint wrapper that runs module initializers and then calls TS main.
    self.build_c_main(entry_file, main_fn, main_sig.ret, &init_order)?;

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
        queue.push_back(dep.file);
      }
    }

    // Use file keys (usually canonical paths) for deterministic iteration order.
    //
    // `FileId` assignment order depends on the host/program load order, which can vary based on
    // resolution and is not a great input for deterministic IR emission.
    let mut files: Vec<(String, FileId)> = visited
      .into_iter()
      .map(|id| (file_label(self.program, id), id))
      .collect();
    files.sort_by(|(a_label, a), (b_label, b)| a_label.cmp(b_label).then_with(|| a.0.cmp(&b.0)));
    files.into_iter().map(|(_, id)| id).collect()
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

  fn init_order(
    &self,
    entry_file: FileId,
    files: &[FileId],
  ) -> Result<Vec<FileId>, Vec<Diagnostic>> {
    let file_set: HashSet<FileId> = files.iter().copied().collect();
    let mut deps: HashMap<FileId, Vec<ModuleDepEdge>> = HashMap::new();
    for file in files {
      let Some(lowered) = self.program.hir_lowered(*file) else {
        continue;
      };
      let out: Vec<ModuleDepEdge> = file_import_deps(self.program, &lowered)
        .into_iter()
        .filter(|dep| file_set.contains(&dep.file))
        .collect();
      deps.insert(*file, out);
    }

    let mut visited = HashSet::<FileId>::new();
    let mut visiting = HashSet::<FileId>::new();
    let mut stack = Vec::<FileId>::new();
    let mut order = Vec::new();
    topo_visit(
      entry_file,
      &deps,
      &mut visited,
      &mut visiting,
      &mut stack,
      &mut order,
    )
    .map_err(|cycle| {
      let formatted: Vec<String> = cycle
        .cycle
        .iter()
        .map(|f| file_label(self.program, *f))
        .collect();
      let cycle_text = if formatted.is_empty() {
        "<unknown cycle>".to_string()
      } else {
        formatted.join(" -> ")
      };
      let mut diagnostic = codes::HIR_CODEGEN_CYCLIC_MODULE_DEPENDENCY.error(
        format!("cyclic module dependency detected: {cycle_text}"),
        cycle.span,
      );

      // Attach labels for each import/re-export edge in the cycle, pointing at the source-level
      // statements that create the runtime dependency. This makes the error actionable while we
      // still reject cycles (until full ESM cycle semantics are implemented in the backend).
      for window in cycle.cycle.windows(2) {
        let [from, to] = window else {
          continue;
        };
        let from = *from;
        let to = *to;
        let Some(edge) = deps
          .get(&from)
          .and_then(|edges| edges.iter().find(|edge| edge.file == to))
        else {
          continue;
        };
        diagnostic.push_label(Label::secondary(
          Span::new(from, edge.span),
          format!("module dependency edge: {} -> {}", file_label(self.program, from), file_label(self.program, to)),
        ));
      }

      vec![diagnostic]
    })?;
    Ok(order)
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

  fn ensure_ts_function(&mut self, def: DefId, sig: TsFunctionSigKind) {
    if self.functions.contains_key(&def) {
      // Ensure signature metadata is available even if the `FunctionValue` was already
      // materialized (e.g. via import aliasing).
      self.function_sigs.entry(def).or_insert(sig);
      return;
    }

    let name = crate::llvm_symbol_for_def(self.program, def);

    let mut params = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
      let ty = match p {
        TsAbiKind::Number => self.f64_ty.into(),
        TsAbiKind::Boolean => self.i1_ty.into(),
        TsAbiKind::Void => unreachable!("void is not a valid parameter ABI kind"),
      };
      params.push(ty);
    }

    let fn_ty = match sig.ret {
      TsAbiKind::Number => self.f64_ty.fn_type(&params, false),
      TsAbiKind::Boolean => self.i1_ty.fn_type(&params, false),
      TsAbiKind::Void => self.context.void_type().fn_type(&params, false),
    };

    let linkage = if self.exported_defs.contains(&def) {
      None
    } else {
      Some(Linkage::Internal)
    };
    let func = self.module.add_function(&name, fn_ty, linkage);
    crate::stack_walking::apply_stack_walking_attrs(self.context, func);
    self.functions.insert(def, func);
    self.function_sigs.insert(def, sig);
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
    let body_id = lowered.root_body();
    let Some(body) = lowered.body(body_id) else {
      return Ok(());
    };
    let types = self.program.check_body(body_id);

    #[derive(Clone, Copy)]
    enum FileInitItem {
      Stmt(StmtId),
      ExportDefaultExpr {
        body: hir_js::BodyId,
        expr: ExprId,
        span: TextRange,
      },
    }

    let mut items: Vec<(TextRange, FileInitItem)> = Vec::new();
    let mut export_default_expr_types: HashMap<hir_js::BodyId, _> = HashMap::new();
    for &stmt in &body.root_stmts {
      let span = body
        .stmts
        .get(stmt.0 as usize)
        .map(|stmt| stmt.span)
        .unwrap_or(body.span);
      items.push((span, FileInitItem::Stmt(stmt)));
    }

    // `export default <expr>` is evaluated at runtime during module init. `hir-js` lowers the
    // default expression into a synthetic top-level body, so we need to explicitly run it here.
    for export in &lowered.hir.exports {
      let hir_js::ExportKind::Default(default) = &export.kind else {
        continue;
      };
      let hir_js::ExportDefaultValue::Expr { expr, body } = &default.value else {
        continue;
      };
      let expr = *expr;
      let body = *body;
      export_default_expr_types
        .entry(body)
        .or_insert_with(|| self.program.check_body(body));
      items.push((
        export.span,
        FileInitItem::ExportDefaultExpr {
          body,
          expr,
          span: export.span,
        },
      ));
    }

    items.sort_by_key(|(span, _)| (span.start, span.end));

    let mut cg = FnCodegen::new(
      self,
      func,
      file,
      body,
      types.as_ref(),
      lowered.names.as_ref(),
      CodegenMode::FileInit,
      TsAbiKind::Void,
    );

    let mut fallthrough = true;
    for (_, item) in items {
      match item {
        FileInitItem::Stmt(stmt) => {
          fallthrough = cg.codegen_stmt(stmt)?;
        }
        FileInitItem::ExportDefaultExpr { body, expr, span } => {
          let span = Span::new(file, span);
          let def = cg.cg.resolve_export_def(file, "default", span)?;
          let global = cg.cg.ensure_global_var(def, span)?;
          let export_body = lowered.body(body).ok_or_else(|| {
            vec![Diagnostic::error(
              "NJS0101",
              "failed to access lowered HIR for export default expression",
              span,
            )]
          })?;
          let export_types = export_default_expr_types.get(&body).ok_or_else(|| {
            vec![Diagnostic::error(
              "NJS0103",
              "missing type information for export default expression",
              span,
            )]
          })?;
          let value =
            cg.with_body(body, export_body, export_types.as_ref(), |cg| cg.codegen_expr(expr))?;
          match (global.kind, value) {
            (TsAbiKind::Number, NativeValue::Number(v)) => {
              cg.builder
                .build_store(global.global.as_pointer_value(), v)
                .expect("failed to build store");
            }
            (TsAbiKind::Boolean, NativeValue::Boolean(v)) => {
              cg.builder
                .build_store(global.global.as_pointer_value(), v)
                .expect("failed to build store");
            }
            (expected, actual) => {
              return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                format!(
                  "export default expression type mismatch (expected {expected:?}, got {got:?})",
                  got = actual.kind()
                ),
                span,
              )]);
            }
          }
        }
      }
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
      vec![codes::HIR_CODEGEN_MISSING_FUNCTION_HIR.error(
        "failed to access lowered HIR for function body",
        Span::new(file, TextRange::new(0, 0)),
      )]
    })?;
    let Some(function_meta) = hir_body.function.as_ref() else {
      return Err(vec![codes::HIR_CODEGEN_MISSING_FUNCTION_META.error(
        "missing function metadata",
        Span::new(file, hir_body.span),
      )]);
    };

    let sig = self
      .function_sigs
      .get(&def)
      .cloned()
      .ok_or_else(|| vec![Diagnostic::error("NJS0102", "missing function signature metadata", Span::new(file, hir_body.span))])?;

    let types = self.program.check_body(body_id);
    let mut cg = FnCodegen::new(
      self,
      func,
      file,
      hir_body,
      types.as_ref(),
      lowered.names.as_ref(),
      CodegenMode::TsFunction,
      sig.ret,
    );

    cg.init_debug_subprogram(def, &sig);

    // Parameters.
    for (idx, param) in function_meta.params.iter().enumerate() {
      let binding = cg
        .cg
        .resolver
        .for_file(file)
        .resolve_pat_ident(hir_body, param.pat)
        .ok_or_else(|| {
          vec![codes::HIR_CODEGEN_INVALID_CONTINUE_OR_BINDING.error(
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

      let Some(&param_kind) = sig.params.get(idx) else {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          "native-js: signature/parameter count mismatch",
          Span::new(file, hir_body.span),
        )]);
      };

      let slot = cg.ensure_local_slot(binding, param_kind, debug_name, Span::new(file, hir_body.span))?;
      let value = func.get_nth_param(idx as u32).expect("missing param");
      let native = match param_kind {
        TsAbiKind::Number => {
          let v = value.into_float_value();
          cg.builder.build_store(slot, v).expect("store param");
          NativeValue::Number(v)
        }
        TsAbiKind::Boolean => {
          let v = value.into_int_value();
          cg.builder.build_store(slot, v).expect("store param");
          NativeValue::Boolean(v)
        }
        TsAbiKind::Void => unreachable!("void is not a valid parameter ABI kind"),
      };

      let pat_span = hir_body
        .pats
        .get(param.pat.0 as usize)
        .map(|pat| pat.span)
        .unwrap_or(hir_body.span);
      cg.dbg_declare_param(binding, debug_name, (idx + 1) as u32, param_kind, slot, pat_span);
      cg.dbg_value(binding, native, pat_span);

      if let Some(name) = cg.pat_ident_name(param.pat) {
        cg.env.bind(name, binding);
      }
    }

    match function_meta.body {
      hir_js::FunctionBody::Expr(expr) => {
        let value = cg.codegen_expr(expr)?;
        match (sig.ret, value) {
          (TsAbiKind::Void, NativeValue::Void) => {
            cg.builder.build_return(None).expect("failed to build return");
          }
          (TsAbiKind::Number, NativeValue::Number(v)) => {
            cg.builder.build_return(Some(&v)).expect("failed to build return");
          }
          (TsAbiKind::Boolean, NativeValue::Boolean(v)) => {
            cg.builder.build_return(Some(&v)).expect("failed to build return");
          }
          (expected, actual) => {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              format!(
                "return type mismatch (expected {expected:?}, got {got:?})",
                got = actual.kind()
              ),
              Span::new(file, hir_body.span),
            )]);
          }
        }
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
          if matches!(sig.ret, TsAbiKind::Void) {
            cg.builder
              .build_return(None)
              .expect("failed to build implicit return");
          } else {
            return Err(vec![codes::HIR_CODEGEN_MISSING_RETURN.error(
              "not all control-flow paths return a value in this codegen subset",
              Span::new(file, hir_body.span),
            )]);
          }
        }
      }
    }

    Ok(())
  }

  fn build_c_main(
    &mut self,
    entry_file: FileId,
    ts_main: FunctionValue<'ctx>,
    ts_main_ret: TsAbiKind,
    init_order: &[FileId],
  ) -> Result<(), Vec<Diagnostic>> {
    let ice_span = Span::new(entry_file, TextRange::new(0, 0));

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

    let abi = RuntimeAbi::new(self.context, &self.module);
    let rt_thread_init = abi.get_or_declare_raw(RuntimeFn::ThreadInit);
    let rt_thread_deinit = abi.get_or_declare_raw(RuntimeFn::ThreadDeinit);
    let _rt_register_shape_table = abi.get_or_declare_raw(RuntimeFn::RegisterShapeTable);

    let call = builder
      .build_call(rt_thread_init, &[self.i32_ty.const_zero().into()], "rt.thread.init")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build call to rt_thread_init: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    // Shape table registration hook.
    //
    // Future work: once the native-js backend emits object shapes, it should also emit a
    // `@__nativejs_shape_table` global containing `RtShapeDescriptor` entries, then call
    // `rt_register_shape_table(ptr, len)` here so the runtime can resolve `RtShapeId` layouts.
    //
    // For now, native-js does not emit any shapes (so registration would be a no-op) and the
    // runtime rejects `len == 0`, so we intentionally skip calling `rt_register_shape_table`.

    for file in init_order {
      if let Some(init) = self.file_inits.get(file).copied() {
        let call = builder.build_call(init, &[], "init").map_err(|e| {
          vec![diagnostics::ice(
            ice_span,
            format!("failed to build call to module init function `{}`: {e}", init.get_name().to_string_lossy()),
          )]
        })?;
        crate::stack_walking::mark_call_notail(call);
      }
    }
    let call = builder
      .build_call(ts_main, &[], "ret")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build call to ts main: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    let ret_val = match ts_main_ret {
      TsAbiKind::Void => self.i32_ty.const_zero(),
      TsAbiKind::Number => {
        let v = call
          .try_as_basic_value()
          .left()
          .expect("non-void TS main should return a value")
          .into_float_value();
        // Note: `fptosi` truncates toward zero. Values outside the i32 range (or NaN) are
        // currently undefined behavior in LLVM IR; we keep this simple conversion for now since
        // native-js uses the `main()` return purely as a process exit code.
        builder
          .build_float_to_signed_int(v, self.i32_ty, "exitcode")
          .expect("failed to build fptosi")
      }
      TsAbiKind::Boolean => {
        let v = call
          .try_as_basic_value()
          .left()
          .expect("non-void TS main should return a value")
          .into_int_value();
        builder
          .build_int_z_extend(v, self.i32_ty, "exitcode")
          .expect("failed to build zext")
      }
    };

    // Chosen convention: the value returned from the exported `main()` becomes the process exit
    // code (truncated by the OS to 8 bits on Unix). For `void`/`undefined`/`never` entrypoints the
    // process exits successfully with code 0.
    //
    // This keeps the wrapper free of libc/vararg calls (e.g. `printf`) which are
    // not rewritten into statepoints by LLVM and therefore violate our GC callsite
    // invariants.
    let call = builder
      .build_call(rt_thread_deinit, &[], "rt.thread.deinit")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build call to rt_thread_deinit: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);
    builder
      .build_return(Some(&ret_val))
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build return in C main wrapper: {e}"))])?;
    Ok(())
  }

  fn ensure_global_var(&mut self, def: DefId, span: Span) -> Result<GlobalSlot<'ctx>, Vec<Diagnostic>> {
    if let Some(existing) = self.globals.get(&def).copied() {
      return Ok(existing);
    }

    let Some(kind) = self.program.def_kind(def) else {
      return Err(vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_DEF_KIND.error(
        "failed to resolve definition kind for global binding",
        span,
      )]);
    };

    match kind {
      typecheck_ts::DefKind::Var(_) | typecheck_ts::DefKind::VarDeclarator(_) => {
        let ts_ty = self.program.type_of_def_interned(def);
        let ts_kind = self.program.type_kind(ts_ty);
        let Some(abi_kind) = TsAbiKind::from_value_type_kind(&ts_kind) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            format!(
              "unsupported global type for native-js ABI (expected number|boolean): {}",
              self.program.display_type(ts_ty)
            ),
            span,
          )]);
        };
        if abi_kind == TsAbiKind::Void {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`void`/`undefined`/`never` global bindings are not supported in native-js codegen",
            span,
          )]);
        }

        let name = crate::llvm_symbol_for_def(self.program, def);
        let global_ty: BasicTypeEnum<'ctx> = match abi_kind {
          TsAbiKind::Number => self.f64_ty.into(),
          TsAbiKind::Boolean => self.i1_ty.into(),
          TsAbiKind::Void => unreachable!(),
        };
        let global = self.module.add_global(global_ty, None, &name);
        match abi_kind {
          TsAbiKind::Number => global.set_initializer(&self.f64_ty.const_float(0.0)),
          TsAbiKind::Boolean => global.set_initializer(&self.i1_ty.const_int(0, false)),
          TsAbiKind::Void => unreachable!(),
        };
        let is_local_to_unit = !self.exported_defs.contains(&def);
        if is_local_to_unit {
          global.set_linkage(Linkage::Internal);
        }

        if let Some(debug) = self.debug.as_mut() {
          let debug_name = self
            .program
            .def_name(def)
            .unwrap_or_else(|| format!("def{}", def.0));
          let def_span = self.program.span_of_def(def).unwrap_or(span);
          debug.declare_global_var(
            self.context,
            self.program,
            def_span.file,
            def_span.range.start,
            &debug_name,
            &name,
            abi_kind,
            global,
            is_local_to_unit,
          );
        }

        let slot = GlobalSlot {
          global,
          kind: abi_kind,
        };
        self.globals.insert(def, slot);
        Ok(slot)
      }
      typecheck_ts::DefKind::Import(import) => match import.target {
        typecheck_ts::ImportTarget::File(target_file) => {
          let target = self.resolve_export_def(target_file, import.original.as_str(), span)?;
          let slot = self.ensure_global_var(target, span)?;
          Ok(slot)
        }
        _ => Err(vec![codes::HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING.error(
          "unresolved import in codegen",
          span,
        )]),
      },
      other => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_GLOBAL_BINDING.error(
        format!("unsupported global binding kind in codegen: {other:?}"),
        span,
      )]),
    }
  }

  fn resolve_export_def(&self, file: FileId, name: &str, span: Span) -> Result<DefId, Vec<Diagnostic>> {
    let (symbol, local_def) = {
      let exports = self.program.exports_of(file);
      let Some(entry) = exports.get(name) else {
        return Err(vec![codes::HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING.error(
          format!("failed to resolve imported binding `{name}`"),
          span,
        )]);
      };
      (entry.symbol, entry.def)
    };

    local_def
      .or_else(|| self.program.symbol_info(symbol).and_then(|info| info.def))
      .ok_or_else(|| {
        vec![codes::HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING.error(
          format!("failed to resolve imported binding `{name}`"),
          span,
        )]
      })
  }

  fn resolve_import_def(&self, def: DefId, span: Span) -> Result<DefId, Vec<Diagnostic>> {
    let mut cur = def;
    let mut seen = HashSet::<DefId>::new();
    loop {
      if !seen.insert(cur) {
        return Err(vec![codes::HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING.error(
          "cyclic import resolution in codegen",
          span,
        )]);
      }

      let Some(kind) = self.program.def_kind(cur) else {
        return Err(vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_DEF_KIND.error(
          "failed to resolve definition kind for imported binding",
          span,
        )]);
      };

      let typecheck_ts::DefKind::Import(import) = kind else {
        return Ok(cur);
      };

      match import.target {
        typecheck_ts::ImportTarget::File(target_file) => {
          cur = self.resolve_export_def(target_file, import.original.as_str(), span)?;
        }
        _ => {
          return Err(vec![codes::HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING.error(
            "unresolved import in codegen",
            span,
          )]);
        }
      }
    }
  }

  fn namespace_import_target(&self, def: DefId) -> Option<FileId> {
    let typecheck_ts::DefKind::Import(import) = self.program.def_kind(def)? else {
      return None;
    };
    if import.original.as_str() != "*" {
      return None;
    }
    match import.target {
      typecheck_ts::ImportTarget::File(target_file) => Some(target_file),
      _ => None,
    }
  }
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
  types: &'a typecheck_ts::BodyCheckResult,
  names: &'a hir_js::NameInterner,
  file: FileId,
  locals: HashMap<BindingId, LocalSlot<'ctx>>,
  env: LocalEnv,
  loop_stack: Vec<LoopContext<'ctx>>,
  mode: CodegenMode,
  return_kind: TsAbiKind,
  debug_subprogram: Option<inkwell::debug_info::DISubprogram<'ctx>>,
  debug_vars: HashMap<BindingId, inkwell::debug_info::DILocalVariable<'ctx>>,
}

impl<'ctx, 'p, 'a> FnCodegen<'ctx, 'p, 'a> {
  fn new(
    cg: &'a mut ProgramCodegen<'ctx, 'p>,
    func: FunctionValue<'ctx>,
    file: FileId,
    body: &'a hir_js::Body,
    types: &'a typecheck_ts::BodyCheckResult,
    names: &'a hir_js::NameInterner,
    mode: CodegenMode,
    return_kind: TsAbiKind,
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
      types,
      names,
      file,
      locals: HashMap::new(),
      env: LocalEnv::new(),
      loop_stack: Vec::new(),
      mode,
      return_kind,
      debug_subprogram: None,
      debug_vars: HashMap::new(),
    }
  }

  fn init_debug_subprogram(&mut self, def: DefId, sig: &TsFunctionSigKind) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };

    let (line, _col) = debuginfo::line_col(
      self.cg.program,
      self.file,
      self.cg
        .program
        .span_of_def(def)
        .map(|s| s.range.start)
        .unwrap_or(0),
    );

    // Prefer the stable LLVM symbol name; it includes the original TS identifier when available.
    let name = crate::llvm_symbol_for_def(self.cg.program, def);

    let sp = debug.create_subprogram(
      self.cg.program,
      self.file,
      &name,
      line,
      sig.ret,
      &sig.params,
      self.func,
    );
    self.debug_subprogram = Some(sp);
  }

  fn dbg_declare_param(
    &mut self,
    binding: BindingId,
    name: &str,
    arg_no: u32,
    kind: TsAbiKind,
    slot: PointerValue<'ctx>,
    span: TextRange,
  ) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };
    let Some(scope) = self.debug_subprogram else {
      return;
    };
    if matches!(kind, TsAbiKind::Void) {
      return;
    }

    let (line, col) = debuginfo::line_col(self.cg.program, self.file, span.start);
    let di_file = debug.file(self.cg.program, self.file);
    let ty = debug.basic_type(kind);
    let var = debug.declare_parameter(
      self.cg.context,
      &self.builder,
      scope,
      di_file,
      line,
      col,
      name,
      arg_no,
      ty,
      slot,
    );
    self.debug_vars.insert(binding, var);
  }

  fn dbg_declare_local(
    &mut self,
    binding: BindingId,
    name: &str,
    kind: TsAbiKind,
    slot: PointerValue<'ctx>,
    span: TextRange,
  ) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };
    let Some(scope) = self.debug_subprogram else {
      return;
    };
    if matches!(kind, TsAbiKind::Void) {
      return;
    }

    let (line, col) = debuginfo::line_col(self.cg.program, self.file, span.start);
    let di_file = debug.file(self.cg.program, self.file);
    let ty = debug.basic_type(kind);
    let var = debug.declare_local(
      self.cg.context,
      &self.builder,
      scope,
      di_file,
      line,
      col,
      name,
      ty,
      slot,
    );
    self.debug_vars.insert(binding, var);
  }

  fn dbg_value(&self, binding: BindingId, value: NativeValue<'ctx>, span: TextRange) {
    let Some(value) = value.as_basic_value() else {
      return;
    };
    let Some(debug) = self.cg.debug.as_ref() else {
      return;
    };
    if !debug.optimized() {
      return;
    }
    let Some(scope) = self.debug_subprogram else {
      return;
    };
    let Some(var) = self.debug_vars.get(&binding).copied() else {
      return;
    };
    let (line, col) = debuginfo::line_col(self.cg.program, self.file, span.start);
    debug.insert_value(self.cg.context, &self.builder, &self.cg.module, scope, var, value, line, col);
  }

  fn dbg_value_locals_from_slots(&self, span: TextRange) {
    let Some(debug) = self.cg.debug.as_ref() else {
      return;
    };
    if !debug.optimized() {
      return;
    }
    // Emit `dbg.value` only for currently visible locals (per lexical scopes in `self.env`), to
    // avoid producing debug loads for out-of-scope variables (which can be uninitialized along some
    // control-flow paths).
    let mut seen_names: HashSet<NameId> = HashSet::new();
    for scope in self.env.scopes.iter().rev() {
      for (&name, &binding) in scope.iter() {
        if !seen_names.insert(name) {
          continue;
        }
        let Some(slot) = self.locals.get(&binding).copied() else {
          continue;
        };
      let loaded = match slot.kind {
        TsAbiKind::Number => NativeValue::Number(
          self
            .builder
            .build_load(self.cg.f64_ty, slot.ptr, "dbg.load")
            .expect("failed to build dbg load")
            .into_float_value(),
        ),
        TsAbiKind::Boolean => NativeValue::Boolean(
          self
            .builder
            .build_load(self.cg.i1_ty, slot.ptr, "dbg.load")
            .expect("failed to build dbg load")
            .into_int_value(),
        ),
        TsAbiKind::Void => continue,
      };
      self.dbg_value(binding, loaded, span);
    }
    }
  }

  fn with_body<R>(
    &mut self,
    _body_id: hir_js::BodyId,
    body: &'a hir_js::Body,
    types: &'a typecheck_ts::BodyCheckResult,
    f: impl FnOnce(&mut Self) -> R,
  ) -> R {
    let prev_body = self.body;
    let prev_types = self.types;
    self.body = body;
    self.types = types;
    let out = f(self);
    self.types = prev_types;
    self.body = prev_body;
    out
  }

  fn stmt(&self, stmt: StmtId) -> Result<&hir_js::Stmt, Vec<Diagnostic>> {
    self.body.stmts.get(stmt.0 as usize).ok_or_else(|| {
      vec![codes::HIR_CODEGEN_STMT_ID_OUT_OF_BOUNDS.error(
        "statement id out of bounds",
        Span::new(self.file, self.body.span),
      )]
    })
  }

  fn expr_data(&self, expr: ExprId) -> Result<&hir_js::Expr, Vec<Diagnostic>> {
    self.body.exprs.get(expr.0 as usize).ok_or_else(|| {
      vec![codes::HIR_CODEGEN_EXPR_ID_OUT_OF_BOUNDS.error(
        "expression id out of bounds",
        Span::new(self.file, self.body.span),
      )]
    })
  }

  fn expr_abi_kind(&self, expr: ExprId, span: Span) -> Result<TsAbiKind, Vec<Diagnostic>> {
    let ty = self.types.expr_type(expr).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0103",
        "missing type information for expression",
        span,
      )]
    })?;
    let kind = self.cg.program.type_kind(ty);
    TsAbiKind::from_value_type_kind(&kind).ok_or_else(|| {
      vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported type for native-js codegen: {}",
          self.cg.program.display_type(ty)
        ),
        span,
      )]
    })
  }

  fn pat_abi_kind(&self, pat: hir_js::PatId, span: Span) -> Result<TsAbiKind, Vec<Diagnostic>> {
    let ty = self.types.pat_type(pat).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0122",
        "missing type information for pattern",
        span,
      )]
    })?;
    let kind = self.cg.program.type_kind(ty);
    TsAbiKind::from_value_type_kind(&kind).ok_or_else(|| {
      vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported type for native-js codegen: {}",
          self.cg.program.display_type(ty)
        ),
        span,
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

  fn ensure_local_slot(
    &mut self,
    binding: BindingId,
    kind: TsAbiKind,
    debug_name: &str,
    span: Span,
  ) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    if let Some(existing) = self.locals.get(&binding).copied() {
      return Ok(existing.ptr);
    }

    if matches!(kind, TsAbiKind::Void) {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "`void`/`undefined`/`never` local bindings are not supported in native-js codegen",
        span,
      )]);
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

    let slot_ty: BasicTypeEnum<'ctx> = match kind {
      TsAbiKind::Number => self.cg.f64_ty.as_basic_type_enum(),
      TsAbiKind::Boolean => self.cg.i1_ty.as_basic_type_enum(),
      TsAbiKind::Void => unreachable!(),
    };

    let slot = self
      .alloca_builder
      .build_alloca(slot_ty, debug_name)
      .expect("failed to build alloca");
    self.locals.insert(binding, LocalSlot { ptr: slot, kind });
    Ok(slot)
  }

  fn is_truthy_i1(&self, v: NativeValue<'ctx>) -> IntValue<'ctx> {
    match v {
      NativeValue::Boolean(v) => v,
      NativeValue::Number(v) => self
        .builder
        .build_float_compare(FloatPredicate::ONE, v, self.cg.f64_ty.const_float(0.0), "truthy")
        .expect("failed to build truthy compare"),
      // `undefined` is falsy in JS; we treat `void` expressions similarly when used in a truthy
      // context.
      NativeValue::Void => self.cg.i1_ty.const_int(0, false),
    }
  }

  /// Convert a `number` (`double`) to a 32-bit integer for bitwise operators.
  ///
  /// This is intended to approximate JS `ToInt32` without invoking UB in LLVM IR:
  /// - `NaN`/`±Infinity` become 0.
  /// - finite values are reduced with `frem` by 2^32 and then truncated toward zero.
  fn number_to_i32(&self, v: FloatValue<'ctx>) -> IntValue<'ctx> {
    let is_nan = self
      .builder
      .build_float_compare(FloatPredicate::UNO, v, v, "is_nan")
      .expect("fcmp uno");
    let is_pos_inf = self
      .builder
      .build_float_compare(
        FloatPredicate::OEQ,
        v,
        self.cg.f64_ty.const_float(f64::INFINITY),
        "is_pos_inf",
      )
      .expect("fcmp oeq inf");
    let is_neg_inf = self
      .builder
      .build_float_compare(
        FloatPredicate::OEQ,
        v,
        self.cg.f64_ty.const_float(f64::NEG_INFINITY),
        "is_neg_inf",
      )
      .expect("fcmp oeq -inf");
    let is_inf = self
      .builder
      .build_or(is_pos_inf, is_neg_inf, "is_inf")
      .expect("or inf");
    let is_bad = self
      .builder
      .build_or(is_nan, is_inf, "is_bad")
      .expect("or bad");

    let two32 = self.cg.f64_ty.const_float(4294967296.0);
    let rem = self
      .builder
      .build_float_rem(v, two32, "frem_2p32")
      .expect("frem");

    let sanitized = self
      .builder
      .build_select(is_bad, self.cg.f64_ty.const_float(0.0), rem, "frem.s")
      .expect("select")
      .into_float_value();

    let as_i64 = self
      .builder
      .build_float_to_signed_int(sanitized, self.cg.i64_ty, "to_i64")
      .expect("fptosi i64");
    self
      .builder
      .build_int_truncate(as_i64, self.cg.i32_ty, "to_i32")
      .expect("trunc i32")
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
      StmtKind::Return(Some(expr)) => {
        let value = self.codegen_expr(expr)?;
        match (self.return_kind, value) {
          (TsAbiKind::Void, NativeValue::Void) => {
            self.builder.build_return(None).expect("failed to build return");
          }
          (TsAbiKind::Number, NativeValue::Number(v)) => {
            self.builder
              .build_return(Some(&v))
              .expect("failed to build return");
          }
          (TsAbiKind::Boolean, NativeValue::Boolean(v)) => {
            self.builder
              .build_return(Some(&v))
              .expect("failed to build return");
          }
          (TsAbiKind::Void, other) => {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_RETURN.error(
              format!(
                "`return <expr>` is only supported when the expression has type void/undefined/never (got {got:?})",
                got = other.kind()
              ),
              span,
            )]);
          }
          (expected, got) => {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              format!(
                "return type mismatch (expected {expected:?}, got {got:?})",
                got = got.kind()
              ),
              span,
            )]);
          }
        }
        Ok(false)
      }
      StmtKind::Return(None) => {
        if matches!(self.return_kind, TsAbiKind::Void) {
          self.builder.build_return(None).expect("failed to build return");
          Ok(false)
        } else {
          Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_RETURN.error(
            "`return` without a value is only supported when the function returns void/undefined/never",
            span,
          )])
        }
      }
      StmtKind::Decl(_) => match self.mode {
        CodegenMode::FileInit => Ok(true),
        CodegenMode::TsFunction => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
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
      StmtKind::Switch { .. } => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
        "`switch` statements are not supported yet",
        span,
      )]),
      StmtKind::Try { .. } => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
        "`try` statements are not supported yet",
        span,
      )]),
      StmtKind::Throw(_) => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
        "`throw` statements are not supported yet",
        span,
      )]),
      StmtKind::ForIn { .. } => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
        "`for-in` / `for-of` loops are not supported yet",
        span,
      )]),
      StmtKind::With { .. } => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
        "`with` statements are not supported yet",
        span,
      )]),
    }
  }

  fn codegen_print_stmt(&mut self, expr: ExprId) -> Result<bool, Vec<Diagnostic>> {
    let (callee, arg_expr, expr_span) = {
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

      (call.callee, arg.expr, expr.span)
    };

    if self.callee_global_intrinsic(callee) != Some(NativeJsIntrinsic::Print) {
      return Ok(false);
    }

    let value = self.codegen_expr(arg_expr)?;
    let NativeValue::Number(value) = value else {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "the `print` intrinsic currently only supports `number` arguments",
        Span::new(self.file, expr_span),
      )]);
    };
    self.emit_print_f64(value);
    Ok(true)
  }

  fn callee_global_intrinsic(&self, expr_id: ExprId) -> Option<NativeJsIntrinsic> {
    let expr = self.expr_data(expr_id).ok()?;
    let ExprKind::Ident(ident) = expr.kind else {
      return None;
    };
    let resolved = self.names.resolve(ident)?;
    let intrinsic = crate::builtins::intrinsic_by_name(resolved)?;

    // `typecheck-ts` currently only indexes symbol occurrences for file-local bindings; global
    // names coming from injected `.d.ts` libs (like native-js intrinsics) generally resolve to
    // `None` here. This matches the strict-subset validator logic: treat the intrinsic as active
    // only when it is not shadowed by a file-local binding named `print`.
    self
      .cg
      .resolver
      .for_file(self.file)
      .resolve_expr_ident(self.body, expr_id)
      .is_none()
      .then_some(intrinsic)
  }

  fn emit_print_f64(&self, value: FloatValue<'ctx>) {
    // NOTE: Do not call `printf` directly from TS-generated functions.
    //
    // The native pipeline runs LLVM's `rewrite-statepoints-for-gc` pass and enforces that
    // TS-generated functions (`__nativejs_def_*` / `__nativejs_file_init_*`) contain no stray
    // non-intrinsic calls after rewrite (except calls to direct `"gc-leaf-function"` callees).
    // See `llvm::passes::verify_no_stray_calls_in_ts_generated_functions`.
    // LLVM does not rewrite varargs calls like `printf` into statepoints reliably, so we route the
    // intrinsic through a small non-TS wrapper.
    let rt_print = declare_rt_print_f64(self.cg.context, &self.cg.module);
    let call = self
      .builder
      .build_call(rt_print, &[value.into()], "native_js_print_f64")
      .expect("failed to build print call");
    crate::stack_walking::mark_call_notail(call);
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
      let code = if label.is_some() {
        codes::HIR_CODEGEN_UNKNOWN_BREAK_LABEL
      } else {
        codes::HIR_CODEGEN_BREAK_OUTSIDE_LOOP
      };
      return Err(vec![code.error(
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
      let code = if label.is_some() {
        codes::HIR_CODEGEN_UNKNOWN_CONTINUE_LABEL
      } else {
        codes::HIR_CODEGEN_INVALID_CONTINUE_OR_BINDING
      };
      return Err(vec![code.error(
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
      _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_LABEL.error(
        "only labeled loops are supported in native-js codegen",
        span,
      )]),
    }
  }

  fn codegen_logical_and(
    &mut self,
    expr: ExprId,
    left: ExprId,
    right: ExprId,
    span: Span,
  ) -> Result<NativeValue<'ctx>, Vec<Diagnostic>> {
    let expected = self.expr_abi_kind(expr, span)?;
    let lhs = self.codegen_expr(left)?;
    let lhs_bb = self
      .builder
      .get_insert_block()
      .expect("logical operator must have an insertion block");
    let rhs_bb = self.cg.context.append_basic_block(self.func, "land.rhs");
    let end_bb = self.cg.context.append_basic_block(self.func, "land.end");

    let cond = self.is_truthy_i1(lhs);
    self
      .builder
      .build_conditional_branch(cond, rhs_bb, end_bb)
      .expect("failed to build logical-and branch");

    self.builder.position_at_end(rhs_bb);
    let rhs = self.codegen_expr(right)?;
    let rhs_end_bb = self
      .builder
      .get_insert_block()
      .expect("rhs codegen must leave an insertion block");
    self
      .builder
      .build_unconditional_branch(end_bb)
      .expect("failed to build logical-and rhs branch");

    self.builder.position_at_end(end_bb);
    self.dbg_value_locals_from_slots(span.range);
    match expected {
      TsAbiKind::Void => {
        if lhs.kind() != TsAbiKind::Void || rhs.kind() != TsAbiKind::Void {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, or `boolean && boolean`",
            span,
          )]);
        }
        Ok(NativeValue::Void)
      }
      TsAbiKind::Number => {
        let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, or `boolean && boolean`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.f64_ty, "land")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        Ok(NativeValue::Number(phi.as_basic_value().into_float_value()))
      }
      TsAbiKind::Boolean => {
        let (NativeValue::Boolean(lhs), NativeValue::Boolean(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, or `boolean && boolean`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.i1_ty, "land")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        Ok(NativeValue::Boolean(phi.as_basic_value().into_int_value()))
      }
    }
  }

  fn codegen_logical_or(
    &mut self,
    expr: ExprId,
    left: ExprId,
    right: ExprId,
    span: Span,
  ) -> Result<NativeValue<'ctx>, Vec<Diagnostic>> {
    let expected = self.expr_abi_kind(expr, span)?;
    let lhs = self.codegen_expr(left)?;
    let lhs_bb = self
      .builder
      .get_insert_block()
      .expect("logical operator must have an insertion block");
    let rhs_bb = self.cg.context.append_basic_block(self.func, "lor.rhs");
    let end_bb = self.cg.context.append_basic_block(self.func, "lor.end");

    let cond = self.is_truthy_i1(lhs);
    self
      .builder
      .build_conditional_branch(cond, end_bb, rhs_bb)
      .expect("failed to build logical-or branch");

    self.builder.position_at_end(rhs_bb);
    let rhs = self.codegen_expr(right)?;
    let rhs_end_bb = self
      .builder
      .get_insert_block()
      .expect("rhs codegen must leave an insertion block");
    self
      .builder
      .build_unconditional_branch(end_bb)
      .expect("failed to build logical-or rhs branch");

    self.builder.position_at_end(end_bb);
    self.dbg_value_locals_from_slots(span.range);
    match expected {
      TsAbiKind::Void => {
        if lhs.kind() != TsAbiKind::Void || rhs.kind() != TsAbiKind::Void {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, or `boolean || boolean`",
            span,
          )]);
        }
        Ok(NativeValue::Void)
      }
      TsAbiKind::Number => {
        let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, or `boolean || boolean`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.f64_ty, "lor")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        Ok(NativeValue::Number(phi.as_basic_value().into_float_value()))
      }
      TsAbiKind::Boolean => {
        let (NativeValue::Boolean(lhs), NativeValue::Boolean(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, or `boolean || boolean`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.i1_ty, "lor")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        Ok(NativeValue::Boolean(phi.as_basic_value().into_int_value()))
      }
    }
  }

  fn codegen_comma(&mut self, left: ExprId, right: ExprId) -> Result<NativeValue<'ctx>, Vec<Diagnostic>> {
    let _ = self.codegen_expr(left)?;
    self.codegen_expr(right)
  }
  fn codegen_if(
    &mut self,
    test: ExprId,
    consequent: StmtId,
    alternate: Option<StmtId>,
  ) -> Result<bool, Vec<Diagnostic>> {
    let test_span = self.expr_data(test)?.span;
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
      self.dbg_value_locals_from_slots(test_span);
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
      self.dbg_value_locals_from_slots(test_span);
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
    let test_span = self.expr_data(test)?.span;
    let cond_bb = self.cg.context.append_basic_block(self.func, "while.cond");
    let body_bb = self.cg.context.append_basic_block(self.func, "while.body");
    let latch_bb = self.cg.context.append_basic_block(self.func, "while.latch");
    let end_bb = self.cg.context.append_basic_block(self.func, "while.end");

    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build branch");

    self.builder.position_at_end(cond_bb);
    self.dbg_value_locals_from_slots(test_span);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(body_bb);
    self.loop_stack.push(LoopContext {
      break_bb: end_bb,
      continue_bb: latch_bb,
      label,
    });
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(latch_bb)
        .expect("failed to build branch");
    }
    self.loop_stack.pop();

    self.builder.position_at_end(latch_bb);
    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build while backedge branch");

    self.builder.position_at_end(end_bb);
    self.dbg_value_locals_from_slots(test_span);
    Ok(true)
  }

  fn codegen_do_while(
    &mut self,
    label: Option<NameId>,
    test: ExprId,
    body: StmtId,
  ) -> Result<bool, Vec<Diagnostic>> {
    let test_span = self.expr_data(test)?.span;
    let body_bb = self.cg.context.append_basic_block(self.func, "do.body");
    let cond_bb = self.cg.context.append_basic_block(self.func, "do.cond");
    let latch_bb = self.cg.context.append_basic_block(self.func, "do.latch");
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
    self.dbg_value_locals_from_slots(test_span);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, latch_bb, end_bb)
      .expect("failed to build conditional branch");

    self.loop_stack.pop();

    self.builder.position_at_end(latch_bb);
    self
      .builder
      .build_unconditional_branch(body_bb)
      .expect("failed to build do-while backedge branch");

    self.builder.position_at_end(end_bb);
    self.dbg_value_locals_from_slots(test_span);
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
      self.dbg_value_locals_from_slots(span.range);
      let cond_i1 = if let Some(test) = test {
        let v = self.codegen_expr(test)?;
        self.is_truthy_i1(v)
      } else {
        self.cg.i1_ty.const_int(1, false)
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
      self.dbg_value_locals_from_slots(span.range);
      if let Some(update) = update {
        let _ = self.codegen_expr(update)?;
      }
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");

      self.builder.position_at_end(end_bb);
      self.dbg_value_locals_from_slots(span.range);
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
        return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_STMT.error(
          "unsupported variable declaration kind in native-js codegen",
          span,
        )]);
      }
    }

    for declarator in decl.declarators.iter() {
      let pat_span = Span::new(
        self.file,
        self
          .body
          .pats
          .get(declarator.pat.0 as usize)
          .map(|pat| pat.span)
          .unwrap_or(span.range),
      );

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
          vec![codes::HIR_CODEGEN_INVALID_CONTINUE_OR_BINDING.error(
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
        return Err(vec![codes::HIR_CODEGEN_VAR_DECL_MISSING_INIT.error(
          "variable declarations must have an initializer in native-js codegen",
          span,
        )]);
      };

      let value = self.codegen_expr(init)?;
      let expected_kind = self.pat_abi_kind(declarator.pat, pat_span)?;
      if value.kind() != expected_kind {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!(
            "variable initializer type mismatch (expected {expected:?}, got {got:?})",
            expected = expected_kind,
            got = value.kind()
          ),
          pat_span,
        )]);
      }

      match binding {
        BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
          let global = self.cg.ensure_global_var(def, pat_span)?;
          if global.kind != expected_kind {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              "global binding type mismatch",
              pat_span,
            )]);
          }

          match value {
            NativeValue::Number(v) => {
              self
                .builder
                .build_store(global.global.as_pointer_value(), v)
                .expect("failed to build store");
            }
            NativeValue::Boolean(v) => {
              self
                .builder
                .build_store(global.global.as_pointer_value(), v)
                .expect("failed to build store");
            }
            NativeValue::Void => unreachable!("void globals are rejected in ensure_global_var"),
          }
        }
        _ => {
          let slot = self.ensure_local_slot(binding, expected_kind, debug_name, pat_span)?;
          match value {
            NativeValue::Number(v) => {
              self.builder.build_store(slot, v).expect("failed to build store");
            }
            NativeValue::Boolean(v) => {
              self.builder.build_store(slot, v).expect("failed to build store");
            }
            NativeValue::Void => unreachable!("void locals are rejected in ensure_local_slot"),
          }

          if let Some(name_id) = self.pat_ident_name(declarator.pat) {
            let name = self.names.resolve(name_id).unwrap_or("local");
            let pat_span = self
              .body
              .pats
              .get(declarator.pat.0 as usize)
              .map(|pat| pat.span)
              .unwrap_or(span.range);
            self.dbg_declare_local(binding, name, expected_kind, slot, pat_span);
            self.dbg_value(binding, value, pat_span);
          }

          if let Some(name) = self.pat_ident_name(declarator.pat) {
            self.env.bind(name, binding);
          }
        }
      }
    }
    Ok(())
  }

  fn slot_for_binding(&mut self, binding: BindingId, span: Span) -> Result<LocalSlot<'ctx>, Vec<Diagnostic>> {
    if let Some(slot) = self.locals.get(&binding).copied() {
      return Ok(slot);
    }
    match binding {
      BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
        let global = self.cg.ensure_global_var(def, span)?;
        Ok(LocalSlot {
          ptr: global.global.as_pointer_value(),
          kind: global.kind,
        })
      }
      _ => Err(vec![codes::HIR_CODEGEN_UNKNOWN_IDENTIFIER.error(
        "use of unknown/unbound identifier in native-js codegen",
        span,
      )]),
    }
  }

  fn member_property_static_name(&mut self, key: &hir_js::ObjectKey, span: Span) -> Result<String, Vec<Diagnostic>> {
    match key {
      hir_js::ObjectKey::Ident(name) => self
        .names
        .resolve(*name)
        .map(|s| s.to_string())
        .ok_or_else(|| vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error("failed to resolve member property name", span)]),
      hir_js::ObjectKey::String(s) => Ok(s.clone()),
      hir_js::ObjectKey::Computed(expr) => {
        let expr = self.expr_data(*expr)?;
        match &expr.kind {
          ExprKind::Literal(Literal::String(s)) => Ok(s.lossy.clone()),
          _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            "unsupported computed property access in this codegen subset (expected a literal string key)",
            span,
          )]),
        }
      }
      _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "unsupported member property syntax in this codegen subset",
        span,
      )]),
    }
  }

  fn resolve_namespace_member_export_def(
    &mut self,
    member: &hir_js::MemberExpr,
    span: Span,
  ) -> Result<DefId, Vec<Diagnostic>> {
    if member.optional {
      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "optional chaining is not supported in this codegen subset",
        span,
      )]);
    }

    let object = self.expr_data(member.object)?;
    let ExprKind::Ident(object_name) = object.kind else {
      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "unsupported member expression receiver in this codegen subset (expected an identifier)",
        Span::new(self.file, object.span),
      )]);
    };

    let object_binding = if let Some(binding) = self.env.resolve(object_name) {
      binding
    } else {
      self
        .cg
        .resolver
        .for_file(self.file)
        .resolve_expr_ident(self.body, member.object)
        .ok_or_else(|| {
          vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_IDENT.error(
            "failed to resolve member expression receiver",
            span,
          )]
        })?
    };

    let BindingId::Def(object_def) = object_binding else {
      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "unsupported member expression receiver in this codegen subset",
        span,
      )]);
    };

    let Some(target_file) = self.cg.namespace_import_target(object_def) else {
      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "unsupported member expression in this codegen subset (only namespace import property access is supported)",
        span,
      )]);
    };

    let export_name = self.member_property_static_name(&member.property, span)?;
    self.cg.resolve_export_def(target_file, export_name.as_str(), span)
  }

  fn codegen_expr(&mut self, expr: ExprId) -> Result<NativeValue<'ctx>, Vec<Diagnostic>> {
    let (kind, span) = {
      let expr_data = self.expr_data(expr)?;
      (expr_data.kind.clone(), Span::new(self.file, expr_data.span))
    };

    match kind {
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => self.codegen_expr(expr),

      ExprKind::Literal(Literal::Number(raw)) => {
        let Some(number) = JsNumber::from_literal(&raw).map(|n| n.0) else {
          return Err(vec![codes::HIR_CODEGEN_INVALID_NUMERIC_LITERAL.error(
            format!("invalid numeric literal `{raw}`"),
            span,
          )]);
        };
        Ok(NativeValue::Number(self.cg.f64_ty.const_float(number)))
      }

      ExprKind::Literal(Literal::Boolean(b)) => Ok(NativeValue::Boolean(self.cg.i1_ty.const_int(u64::from(b), false))),

      ExprKind::Unary { op, expr } => {
        let inner = self.codegen_expr(expr)?;
        match op {
          UnaryOp::Plus => match inner {
            NativeValue::Number(v) => Ok(NativeValue::Number(v)),
            _ => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error("unary `+` expects a number", span)]),
          },
          UnaryOp::Minus => match inner {
            NativeValue::Number(v) => Ok(NativeValue::Number(
              self
                .builder
                .build_float_neg(v, "neg")
                .expect("failed to build negation"),
            )),
            _ => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error("unary `-` expects a number", span)]),
          },
          UnaryOp::Not => {
            let truthy = self.is_truthy_i1(inner);
            let not = self
              .builder
              .build_not(truthy, "not")
              .expect("failed to build not");
            Ok(NativeValue::Boolean(not))
          }
          UnaryOp::BitNot => match inner {
            NativeValue::Number(v) => {
              let i32_v = self.number_to_i32(v);
              let not = self.builder.build_not(i32_v, "bitnot").expect("bitnot");
              let out = self
                .builder
                .build_signed_int_to_float(not, self.cg.f64_ty, "bitnot.f")
                .expect("sitofp");
              Ok(NativeValue::Number(out))
            }
            _ => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error("unary `~` expects a number", span)]),
          },
          _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_UNARY_OP.error(
            format!("unsupported unary operator `{op:?}`"),
            span,
          )]),
        }
      }

      ExprKind::Binary { op, left, right } => {
        match op {
          BinaryOp::LogicalAnd => return self.codegen_logical_and(expr, left, right, span),
          BinaryOp::LogicalOr => return self.codegen_logical_or(expr, left, right, span),
          BinaryOp::Comma => return self.codegen_comma(left, right),
          _ => {}
        }

        let lhs = self.codegen_expr(left)?;
        let rhs = self.codegen_expr(right)?;

        match op {
          // Arithmetic on numbers.
          BinaryOp::Add
          | BinaryOp::Subtract
          | BinaryOp::Multiply
          | BinaryOp::Divide
          | BinaryOp::Remainder => {
            let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
              return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                "numeric operators currently require `number` operands",
                span,
              )]);
            };
            let out = match op {
              BinaryOp::Add => self.builder.build_float_add(lhs, rhs, "fadd").expect("fadd"),
              BinaryOp::Subtract => self.builder.build_float_sub(lhs, rhs, "fsub").expect("fsub"),
              BinaryOp::Multiply => self.builder.build_float_mul(lhs, rhs, "fmul").expect("fmul"),
              BinaryOp::Divide => self.builder.build_float_div(lhs, rhs, "fdiv").expect("fdiv"),
              BinaryOp::Remainder => self.builder.build_float_rem(lhs, rhs, "frem").expect("frem"),
              _ => unreachable!(),
            };
            Ok(NativeValue::Number(out))
          }

          // Bitwise operators on numbers (`ToInt32`).
          BinaryOp::BitAnd
          | BinaryOp::BitOr
          | BinaryOp::BitXor
          | BinaryOp::ShiftLeft
          | BinaryOp::ShiftRight
          | BinaryOp::ShiftRightUnsigned => {
            let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
              return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                "bitwise operators currently require `number` operands",
                span,
              )]);
            };
            let lhs_i32 = self.number_to_i32(lhs);
            let rhs_i32 = self.number_to_i32(rhs);
            let shamt = self
              .builder
              .build_and(rhs_i32, self.cg.i32_ty.const_int(31, false), "shamt")
              .expect("shamt");
            let out_i32 = match op {
              BinaryOp::BitAnd => self.builder.build_and(lhs_i32, rhs_i32, "and").expect("and"),
              BinaryOp::BitOr => self.builder.build_or(lhs_i32, rhs_i32, "or").expect("or"),
              BinaryOp::BitXor => self.builder.build_xor(lhs_i32, rhs_i32, "xor").expect("xor"),
              BinaryOp::ShiftLeft => self
                .builder
                .build_left_shift(lhs_i32, shamt, "shl")
                .expect("shl"),
              BinaryOp::ShiftRight => self
                .builder
                .build_right_shift(lhs_i32, shamt, true, "shr")
                .expect("shr"),
              BinaryOp::ShiftRightUnsigned => self
                .builder
                .build_right_shift(lhs_i32, shamt, false, "shr_u")
                .expect("shr_u"),
              _ => unreachable!(),
            };

            let out = if matches!(op, BinaryOp::ShiftRightUnsigned) {
              self
                .builder
                .build_unsigned_int_to_float(out_i32, self.cg.f64_ty, "uitofp")
                .expect("uitofp")
            } else {
              self
                .builder
                .build_signed_int_to_float(out_i32, self.cg.f64_ty, "sitofp")
                .expect("sitofp")
            };
            Ok(NativeValue::Number(out))
          }

          // Comparisons.
          BinaryOp::LessThan | BinaryOp::LessEqual | BinaryOp::GreaterThan | BinaryOp::GreaterEqual => {
            let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
              return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                "ordering comparisons currently require `number` operands",
                span,
              )]);
            };
            let pred = match op {
              BinaryOp::LessThan => FloatPredicate::OLT,
              BinaryOp::LessEqual => FloatPredicate::OLE,
              BinaryOp::GreaterThan => FloatPredicate::OGT,
              BinaryOp::GreaterEqual => FloatPredicate::OGE,
              _ => unreachable!(),
            };
            let cmp = self
              .builder
              .build_float_compare(pred, lhs, rhs, "fcmp")
              .expect("fcmp");
            Ok(NativeValue::Boolean(cmp))
          }

          BinaryOp::Equality | BinaryOp::StrictEquality | BinaryOp::Inequality | BinaryOp::StrictInequality => {
            match (lhs, rhs) {
              (NativeValue::Number(lhs), NativeValue::Number(rhs)) => {
                let pred = match op {
                  BinaryOp::Equality | BinaryOp::StrictEquality => FloatPredicate::OEQ,
                  BinaryOp::Inequality | BinaryOp::StrictInequality => FloatPredicate::UNE,
                  _ => unreachable!(),
                };
                let cmp = self
                  .builder
                  .build_float_compare(pred, lhs, rhs, "fcmp")
                  .expect("fcmp");
                Ok(NativeValue::Boolean(cmp))
              }
              (NativeValue::Boolean(lhs), NativeValue::Boolean(rhs)) => {
                let pred = match op {
                  BinaryOp::Equality | BinaryOp::StrictEquality => IntPredicate::EQ,
                  BinaryOp::Inequality | BinaryOp::StrictInequality => IntPredicate::NE,
                  _ => unreachable!(),
                };
                let cmp = self
                  .builder
                  .build_int_compare(pred, lhs, rhs, "icmp")
                  .expect("icmp");
                Ok(NativeValue::Boolean(cmp))
              }
              _ => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                "equality comparisons currently require both operands to have the same primitive type",
                span,
              )]),
            }
          }

          _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_BINARY_OP.error(
            format!("unsupported binary operator `{op:?}`"),
            span,
          )]),
        }
      }
      ExprKind::Member(member) => {
        let export_def = self.resolve_namespace_member_export_def(&member, span)?;
        let resolved = self.cg.resolve_import_def(export_def, span)?;
        if matches!(
          self.cg.program.def_kind(resolved),
          Some(typecheck_ts::DefKind::Function(_))
        ) {
          return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            "function-valued namespace member access is only supported in call position in this codegen subset",
            span,
          )]);
        }

        let global = self.cg.ensure_global_var(export_def, span)?;
        match global.kind {
          TsAbiKind::Number => Ok(NativeValue::Number(
            self
              .builder
              .build_load(self.cg.f64_ty, global.global.as_pointer_value(), "ns.load")
              .expect("failed to build load")
              .into_float_value(),
          )),
          TsAbiKind::Boolean => Ok(NativeValue::Boolean(
            self
              .builder
              .build_load(self.cg.i1_ty, global.global.as_pointer_value(), "ns.load")
              .expect("failed to build load")
              .into_int_value(),
          )),
          TsAbiKind::Void => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`void` values are not supported in this context",
            span,
          )]),
        }
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
            .ok_or_else(|| vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_IDENT.error("failed to resolve identifier", span)])?
        };

        if let BindingId::Def(def) = binding {
          if self.cg.namespace_import_target(def).is_some() {
            let label = self.names.resolve(name).unwrap_or("<namespace>");
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
              format!(
                "unsupported use of namespace import `{label}` in native-js codegen (only direct property access like `{label}.foo` is supported)"
              ),
              span,
            )]);
          }
        }

        let slot = match binding {
          b if self.locals.contains_key(&b) => self.locals.get(&b).copied().unwrap(),
          BindingId::Def(def) if is_toplevel_def(self.cg.program, def) => {
            let global = self.cg.ensure_global_var(def, span)?;
            LocalSlot {
              ptr: global.global.as_pointer_value(),
              kind: global.kind,
            }
          }
          _ => {
            let label = self.names.resolve(name).unwrap_or("<unknown>");
            return Err(vec![codes::HIR_CODEGEN_UNKNOWN_IDENTIFIER.error(
              format!("unknown identifier `{label}` in native-js codegen"),
              span,
            )]);
          }
        };

        let out = match slot.kind {
          TsAbiKind::Number => NativeValue::Number(
            self
              .builder
              .build_load(self.cg.f64_ty, slot.ptr, "load")
              .expect("failed to build load")
              .into_float_value(),
          ),
          TsAbiKind::Boolean => NativeValue::Boolean(
            self
              .builder
              .build_load(self.cg.i1_ty, slot.ptr, "load")
              .expect("failed to build load")
              .into_int_value(),
          ),
          TsAbiKind::Void => {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              "`void` values are not supported in this context",
              span,
            )]);
          }
        };

        self.dbg_value(binding, out, span.range);
        Ok(out)
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
                vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
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
              vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                "unsupported assignment target",
                Span::new(self.file, pat_span),
              )]
            })?
        };

        let slot = self.slot_for_binding(binding, span)?;
        let rhs = self.codegen_expr(value)?;
        let out = self.codegen_assignment_to_slot(slot, span, &op, rhs)?;
        self.dbg_value(binding, out, span.range);
        Ok(out)
      }

      ExprKind::Update { op, expr, prefix } => {
        let (name, inner_span) = {
          let inner = self.expr_data(expr)?;
          let ExprKind::Ident(name) = inner.kind else {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
              "unsupported update target (expected identifier)",
              Span::new(self.file, inner.span),
            )]);
          };
          (name, inner.span)
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
              vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_IDENT.error(
                "failed to resolve update target",
                Span::new(self.file, inner_span),
              )]
            })?
        };

        let slot = self.slot_for_binding(binding, Span::new(self.file, inner_span))?;
        if slot.kind != TsAbiKind::Number {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "update operators (`++`/`--`) currently require `number` bindings",
            Span::new(self.file, inner_span),
          )]);
        }

        let old = self
          .builder
          .build_load(self.cg.f64_ty, slot.ptr, "load")
          .expect("failed to build load")
          .into_float_value();
        let one = self.cg.f64_ty.const_float(1.0);
        let new = match op {
          UpdateOp::Increment => self.builder.build_float_add(old, one, "inc").expect("inc"),
          UpdateOp::Decrement => self.builder.build_float_sub(old, one, "dec").expect("dec"),
        };
        self
          .builder
          .build_store(slot.ptr, new)
          .expect("failed to build store");
        self.dbg_value(binding, NativeValue::Number(new), inner_span);
        Ok(NativeValue::Number(if prefix { new } else { old }))
      }

      ExprKind::Call(call) => {
        if call.optional || call.is_new {
          return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
            "unsupported call syntax in codegen",
            span,
          )]);
        }

        let callee_expr = self
          .body
          .exprs
          .get(call.callee.0 as usize)
          .ok_or_else(|| vec![codes::HIR_CODEGEN_EXPR_ID_OUT_OF_BOUNDS.error("callee id out of bounds", span)])?;

        let (def, resolved) = match &callee_expr.kind {
          ExprKind::Ident(_) => {
            let binding = self
              .cg
              .resolver
              .for_file(self.file)
              .resolve_expr_ident(self.body, call.callee)
              .ok_or_else(|| vec![codes::HIR_CODEGEN_FAILED_TO_RESOLVE_IDENT.error("failed to resolve call callee", span)])?;
            let BindingId::Def(def) = binding else {
              return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
                "callee must resolve to a global function definition",
                span,
              )]);
            };

            let resolved = self.cg.resolve_import_def(def, span)?;
            (def, resolved)
          }
          ExprKind::Member(member) => {
            let def = self.resolve_namespace_member_export_def(member, span)?;
            let resolved = self.cg.resolve_import_def(def, span)?;
            if !matches!(
              self.cg.program.def_kind(resolved),
              Some(typecheck_ts::DefKind::Function(_))
            ) {
              return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
                "callee must resolve to a global function definition",
                span,
              )]);
            }
            (def, resolved)
          }
          _ => {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
              "only direct identifier calls and namespace import member calls are supported in this codegen subset",
              span,
            )]);
          }
        };
        let Some(target) = self.cg.functions.get(&resolved).copied() else {
          return Err(vec![codes::HIR_CODEGEN_INVALID_CALL.error(
            "call to unknown function in codegen",
            span,
          )]);
        };
        let sig = self
          .cg
          .function_sigs
          .get(&resolved)
          .cloned()
          .ok_or_else(|| vec![Diagnostic::error("NJS0102", "missing function signature metadata", span)])?;

        if def != resolved {
          self.cg.functions.entry(def).or_insert(target);
        }

        for arg in &call.args {
          if arg.spread {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
              "spread arguments are not supported in this codegen subset",
              span,
            )]);
          }
        }
        let expected = target.count_params() as usize;
        if call.args.len() != expected {
          return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX.error(
            format!(
              "wrong number of arguments (expected {expected}, got {})",
              call.args.len()
            ),
            span,
          )]);
        }

        let mut args = Vec::with_capacity(call.args.len());
        for (idx, arg) in call.args.iter().enumerate() {
          let value = self.codegen_expr(arg.expr)?;
          let Some(&expected_kind) = sig.params.get(idx) else {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              "native-js: signature/parameter count mismatch",
              span,
            )]);
          };
          if value.kind() != expected_kind {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              format!(
                "argument type mismatch (param #{idx} expects {expected:?}, got {got:?})",
                expected = expected_kind,
                got = value.kind()
              ),
              span,
            )]);
          }
          args.push(
            value
              .into_basic_metadata()
              .ok_or_else(|| vec![Diagnostic::error("NJS0145", "void argument not supported", span)])?,
          );
        }

        let call = self
          .builder
          .build_call(target, &args, "call")
          .expect("failed to build call");
        crate::stack_walking::mark_call_notail(call);

        match sig.ret {
          TsAbiKind::Void => Ok(NativeValue::Void),
          TsAbiKind::Number => Ok(NativeValue::Number(
            call
              .try_as_basic_value()
              .left()
              .expect("non-void call should return a value")
              .into_float_value(),
          )),
          TsAbiKind::Boolean => Ok(NativeValue::Boolean(
            call
              .try_as_basic_value()
              .left()
              .expect("non-void call should return a value")
              .into_int_value(),
          )),
        }
      }

      _ => Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        "unsupported expression in native-js codegen",
        span,
      )]),
    }
  }

  fn codegen_assignment_to_slot(
    &self,
    slot: LocalSlot<'ctx>,
    span: Span,
    op: &AssignOp,
    rhs: NativeValue<'ctx>,
  ) -> Result<NativeValue<'ctx>, Vec<Diagnostic>> {
    if rhs.kind() != slot.kind {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "assignment type mismatch (expected {expected:?}, got {got:?})",
          expected = slot.kind,
          got = rhs.kind()
        ),
        span,
      )]);
    }

    let out = match op {
      AssignOp::Assign => rhs,
      AssignOp::AddAssign
      | AssignOp::SubAssign
      | AssignOp::MulAssign
      | AssignOp::DivAssign
      | AssignOp::RemAssign => {
        if slot.kind != TsAbiKind::Number {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "compound assignments are only supported for `number` bindings",
            span,
          )]);
        }
        let NativeValue::Number(rhs) = rhs else {
          unreachable!("checked kind above");
        };
        let cur = self
          .builder
          .build_load(self.cg.f64_ty, slot.ptr, "load")
          .expect("failed to build load")
          .into_float_value();
        let new = match op {
          AssignOp::AddAssign => self.builder.build_float_add(cur, rhs, "addassign").expect("addassign"),
          AssignOp::SubAssign => self.builder.build_float_sub(cur, rhs, "subassign").expect("subassign"),
          AssignOp::MulAssign => self.builder.build_float_mul(cur, rhs, "mulassign").expect("mulassign"),
          AssignOp::DivAssign => self.builder.build_float_div(cur, rhs, "divassign").expect("divassign"),
          AssignOp::RemAssign => self.builder.build_float_rem(cur, rhs, "remassign").expect("remassign"),
          _ => unreachable!(),
        };
        NativeValue::Number(new)
      }
      _ => {
        return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_OP.error(
          format!("unsupported assignment operator `{op:?}`"),
          span,
        )]);
      }
    };

    match out {
      NativeValue::Number(v) => {
        self
          .builder
          .build_store(slot.ptr, v)
          .expect("failed to build store");
      }
      NativeValue::Boolean(v) => {
        self
          .builder
          .build_store(slot.ptr, v)
          .expect("failed to build store");
      }
      NativeValue::Void => unreachable!("void values are not assignable"),
    }

    Ok(out)
  }
}

#[derive(Clone, Copy, Debug)]
struct ModuleDepEdge {
  file: FileId,
  span: TextRange,
}

#[derive(Clone, Debug)]
struct ModuleCycle {
  cycle: Vec<FileId>,
  span: Span,
}

fn file_label(program: &Program, file: FileId) -> String {
  program
    .file_key(file)
    .map(|k| k.to_string())
    .unwrap_or_else(|| format!("file{}", file.0))
}

fn is_runtime_import(es: &hir_js::ImportEs) -> bool {
  if es.is_type_only {
    return false;
  }
  if es.default.is_some() || es.namespace.is_some() {
    return true;
  }
  if es.named.iter().any(|spec| !spec.is_type_only) {
    return true;
  }
  // `import "foo";` or `import {} from "foo";`
  es.named.is_empty()
}

fn file_import_deps(program: &Program, lowered: &hir_js::LowerResult) -> Vec<ModuleDepEdge> {
  // Keep module dependencies in the same order as the source-level module
  // requests (`import ...` and `export ... from ...` statements). This matches JS
  // module evaluation semantics and provides deterministic initialization order
  // for sibling dependencies.
  let from = lowered.hir.file;
  #[derive(Clone, Copy)]
  struct ModuleRequest<'a> {
    span: TextRange,
    specifier: &'a str,
  }

  let mut module_requests: Vec<ModuleRequest<'_>> = Vec::new();
  for import in &lowered.hir.imports {
    let ImportKind::Es(es) = &import.kind else {
      continue;
    };
    if !is_runtime_import(es) {
      continue;
    }
    module_requests.push(ModuleRequest {
      span: import.span,
      specifier: es.specifier.value.as_str(),
    });
  }

  // Re-exports like `export { x } from "./dep"` or `export * from "./dep"` also
  // create module dependencies that must be initialized at runtime.
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
        module_requests.push(ModuleRequest {
          span: export.span,
          specifier: source.value.as_str(),
        });
      }
      hir_js::ExportKind::ExportAll(all) => {
        if all.is_type_only {
          continue;
        }
        module_requests.push(ModuleRequest {
          span: export.span,
          specifier: all.source.value.as_str(),
        });
      }
      _ => {}
    }
  }

  module_requests.sort_by_key(|req| (req.span.start, req.span.end));

  let mut deps = Vec::new();
  let mut seen = HashSet::<FileId>::new();
  for req in module_requests {
    let Some(dep) = program.resolve_module(from, req.specifier) else {
      continue;
    };
    if program
      .hir_lowered(dep)
      .is_some_and(|lowered| matches!(lowered.hir.file_kind, FileKind::Dts))
    {
      continue;
    }
    if seen.insert(dep) {
      deps.push(ModuleDepEdge {
        file: dep,
        span: req.span,
      });
    }
  }

  deps
}

fn topo_visit(
  file: FileId,
  deps: &HashMap<FileId, Vec<ModuleDepEdge>>,
  visited: &mut HashSet<FileId>,
  visiting: &mut HashSet<FileId>,
  stack: &mut Vec<FileId>,
  out: &mut Vec<FileId>,
) -> Result<(), ModuleCycle> {
  if visited.contains(&file) {
    return Ok(());
  }
  // `visiting` is the recursion stack / in-progress set.
  visiting.insert(file);
  stack.push(file);
  if let Some(children) = deps.get(&file) {
    for dep in children {
      if visited.contains(&dep.file) {
        continue;
      }
      if visiting.contains(&dep.file) {
        let idx = stack.iter().position(|f| *f == dep.file).unwrap_or(0);
        let mut cycle = stack[idx..].to_vec();
        cycle.push(dep.file);
        return Err(ModuleCycle {
          cycle,
          span: Span::new(file, dep.span),
        });
      }
      topo_visit(dep.file, deps, visited, visiting, stack, out)?;
    }
  }
  stack.pop();
  visiting.remove(&file);
  visited.insert(file);
  out.push(file);
  Ok(())
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

fn declare_rt_print_f64<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("rt_print_f64") {
    return existing;
  }

  let void_ty = context.void_type();
  let f64_ty = context.f64_type();
  let func = module.add_function(
    "rt_print_f64",
    void_ty.fn_type(&[f64_ty.into()], false),
    Some(Linkage::Internal),
  );
  // Keep frame pointers / disable tail calls for stack walking, but do not mark
  // this helper as GC-managed. It must not contain statepoints/stackmaps.
  crate::stack_walking::apply_stack_walking_frame_attrs(context, func);

  let builder = context.create_builder();
  let bb = context.append_basic_block(func, "entry");
  builder.position_at_end(bb);

  let printf = declare_printf(context, module);
  let fmt = builder
    // Match the parse-js backend's debug-friendly number formatting.
    .build_global_string_ptr("%.15g\n", "native_js_print_fmt")
    .expect("failed to create printf format string");
  let value = func
    .get_nth_param(0)
    .expect("missing print arg")
    .into_float_value();
  let call = builder
    .build_call(
      printf,
      &[fmt.as_pointer_value().into(), value.into()],
      "native_js_print",
    )
    .expect("failed to build printf call");
  crate::stack_walking::mark_call_notail(call);
  builder.build_return(None).expect("failed to build return");

  func
}

fn declare_printf<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("printf") {
    return existing;
  }
  let i32_ty = context.i32_type();
  let ptr_ty = context.ptr_type(AddressSpace::default());
  module.add_function("printf", i32_ty.fn_type(&[ptr_ty.into()], true), None)
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
