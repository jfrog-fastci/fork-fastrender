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
use crate::array_abi;
use crate::llvm::gc;
use crate::resolve::BindingId;
use crate::runtime_abi::{RuntimeAbi, RuntimeFn};
use crate::shapes;
use crate::strict::Entrypoint;
use crate::codes;
use crate::Resolver;
mod debuginfo;
use diagnostics::{Diagnostic, Label, Span, TextRange};
use hir_js::{
  ArrayElement, AssignOp, BinaryOp, ExprId, ExprKind, FileKind, ForInit, ImportKind, Literal, NameId, ObjectKey,
  ObjectProperty, PatKind, StmtId, StmtKind, UnaryOp, UpdateOp, VarDecl, VarDeclKind,
};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicType, BasicTypeEnum, FloatType, IntType, PointerType};
use inkwell::values::{
  BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, GlobalValue, IntValue,
  PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};
use parse_js::num::JsNumber;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use typecheck_ts::{DefId, FileId, Program, TypeKindSummary};
use types_ts_interned as tti;

pub struct CodegenOptions {
  pub module_name: String,
  /// Whether to emit DWARF debug info metadata (`llvm.dbg.*` intrinsics, `!DI*` nodes, etc).
  ///
  /// This is primarily used by the typechecked/native pipeline (`native-js-cli`, `native-js`).
  pub debug: bool,
  /// Remap source paths embedded in emitted debug info (DWARF).
  ///
  /// Deterministic precedence: the first matching mapping wins.
  pub debug_path_prefix_map: Vec<(PathBuf, PathBuf)>,
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
      debug_path_prefix_map: Vec::new(),
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
  /// Runtime-native interned string id (`InternedId`, `u32`).
  String,
  Void,
  /// GC-managed pointer value (`ptr addrspace(1)`).
  ///
  /// Used for local/global bindings of GC-managed types (arrays/tuples/objects) in the checked
  /// pipeline.
  GcPtr,
}

impl TsAbiKind {
  fn from_value_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::String | TypeKindSummary::StringLiteral(_) => Some(Self::String),
      TypeKindSummary::Array { .. }
      | TypeKindSummary::Tuple { .. }
      | TypeKindSummary::Object
      | TypeKindSummary::EmptyObject => Some(Self::GcPtr),
      TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never => Some(Self::Void),
      _ => None,
    }
  }

  fn from_param_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::String | TypeKindSummary::StringLiteral(_) => Some(Self::String),
      TypeKindSummary::Object
      | TypeKindSummary::EmptyObject
      | TypeKindSummary::Array { .. }
      | TypeKindSummary::Tuple { .. } => Some(Self::GcPtr),
      _ => None,
    }
  }

  fn from_return_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::String | TypeKindSummary::StringLiteral(_) => Some(Self::String),
      TypeKindSummary::Object
      | TypeKindSummary::EmptyObject
      | TypeKindSummary::Array { .. }
      | TypeKindSummary::Tuple { .. } => Some(Self::GcPtr),
      TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never => Some(Self::Void),
      _ => None,
    }
  }
}

fn number_literal_to_i64(raw: &str) -> Option<i64> {
  let n = JsNumber::from_literal(raw).map(|n| n.0)?;
  if !n.is_finite() || n.fract() != 0.0 {
    return None;
  }
  if n < i64::MIN as f64 || n > i64::MAX as f64 {
    return None;
  }
  Some(n as i64)
}

fn canonical_decimal_string_to_i64(raw: &str) -> Option<i64> {
  if raw == "0" {
    return Some(0);
  }
  if raw.is_empty() || raw.starts_with('0') || !raw.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  raw.parse().ok()
}

#[derive(Clone, Copy, Debug)]
enum NativeValue<'ctx> {
  Number(FloatValue<'ctx>),
  Boolean(IntValue<'ctx>),
  String(IntValue<'ctx>),
  GcPtr(PointerValue<'ctx>),
  Void,
}

impl<'ctx> NativeValue<'ctx> {
  fn kind(self) -> TsAbiKind {
    match self {
      NativeValue::Number(_) => TsAbiKind::Number,
      NativeValue::Boolean(_) => TsAbiKind::Boolean,
      NativeValue::String(_) => TsAbiKind::String,
      NativeValue::GcPtr(_) => TsAbiKind::GcPtr,
      NativeValue::Void => TsAbiKind::Void,
    }
  }

  fn as_basic_value(self) -> Option<BasicValueEnum<'ctx>> {
    match self {
      NativeValue::Number(v) => Some(v.into()),
      NativeValue::Boolean(v) => Some(v.into()),
      NativeValue::String(v) => Some(v.into()),
      NativeValue::GcPtr(v) => Some(v.into()),
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

#[derive(Clone, Copy)]
struct StringLiteralGlobals<'ctx> {
  bytes: GlobalValue<'ctx>,
  id: GlobalValue<'ctx>,
  len: u64,
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
          "unsupported parameter type for native-js ABI (expected number|boolean|string|gc pointer (array/tuple/object)): {}",
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
        "unsupported return type for native-js ABI (expected number|boolean|string|void|gc pointer (array/tuple/object)): {}",
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
  gc_ptr_ty: PointerType<'ctx>,
  debug_function_attrs: bool,
  program: &'p Program,
  resolver: Resolver<'p>,
  exported_defs: HashSet<DefId>,
  globals: HashMap<DefId, GlobalSlot<'ctx>>,
  /// Globals that store GC-managed pointers (`ptr addrspace(1)`) and must be registered as global
  /// roots with the runtime.
  gc_root_globals: Vec<GlobalValue<'ctx>>,
  shapes: shapes::ShapeTableBuilder<'ctx>,
  shape_table: Option<shapes::EmittedShapeTable<'ctx>>,
  functions: HashMap<DefId, FunctionValue<'ctx>>,
  function_sigs: HashMap<DefId, TsFunctionSigKind>,
  file_inits: HashMap<FileId, FunctionValue<'ctx>>,
  string_literals: HashMap<FileId, BTreeMap<String, StringLiteralGlobals<'ctx>>>,
  debug: Option<debuginfo::CodegenDebug<'ctx>>,
}

impl<'ctx, 'p> ProgramCodegen<'ctx, 'p> {
  fn new(context: &'ctx Context, program: &'p Program, entry_file: FileId, options: &CodegenOptions) -> Self {
    let module = context.create_module(&options.module_name);
    let debug = options
      .debug
      .then(|| {
        debuginfo::CodegenDebug::new(
          &module,
          program,
          entry_file,
          options.opt_level,
          &options.debug_path_prefix_map,
        )
      });
    Self {
      context,
      module,
      f64_ty: context.f64_type(),
      i32_ty: context.i32_type(),
      i64_ty: context.i64_type(),
      i1_ty: context.bool_type(),
      gc_ptr_ty: gc::gc_ptr_type(context),
      debug_function_attrs: options.debug && options.opt_level == crate::OptLevel::O0,
      program,
      resolver: Resolver::new(program),
      exported_defs: HashSet::new(),
      globals: HashMap::new(),
      gc_root_globals: Vec::new(),
      shapes: shapes::ShapeTableBuilder::new(),
      shape_table: None,
      functions: HashMap::new(),
      function_sigs: HashMap::new(),
      file_inits: HashMap::new(),
      string_literals: HashMap::new(),
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

    // Pre-scan and declare per-file string literal globals so:
    // - file initializers can intern+pin string literals at startup, and
    // - expression codegen can lower string literals to a cheap `i32` load.
    for file in &files {
      self.collect_string_literals_for_file(*file);
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

    // Emit the runtime shape table (if any) after codegen has discovered all used object shapes.
    self.shape_table = self
      .shapes
      .emit_shape_table(self.context, &self.module)?;

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
    if self.debug_function_attrs {
      crate::stack_walking::apply_debug_function_attrs(self.context, func);
    }
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
        TsAbiKind::String => self.i32_ty.into(),
        TsAbiKind::GcPtr => self.gc_ptr_ty.into(),
        TsAbiKind::Void => unreachable!("void is not a valid parameter ABI kind"),
      };
      params.push(ty);
    }

    let fn_ty = match sig.ret {
      TsAbiKind::Number => self.f64_ty.fn_type(&params, false),
      TsAbiKind::Boolean => self.i1_ty.fn_type(&params, false),
      TsAbiKind::String => self.i32_ty.fn_type(&params, false),
      TsAbiKind::Void => self.context.void_type().fn_type(&params, false),
      TsAbiKind::GcPtr => self.gc_ptr_ty.fn_type(&params, false),
    };

    let linkage = if self.exported_defs.contains(&def) {
      None
    } else {
      Some(Linkage::Internal)
    };
    let func = self.module.add_function(&name, fn_ty, linkage);
    crate::stack_walking::apply_stack_walking_attrs(self.context, func);
    if self.debug_function_attrs {
      crate::stack_walking::apply_debug_function_attrs(self.context, func);
    }
    self.functions.insert(def, func);
    self.function_sigs.insert(def, sig);
  }

  fn collect_string_literals_for_file(&mut self, file: FileId) {
    if self.string_literals.contains_key(&file) {
      return;
    }
    let Some(lowered) = self.program.hir_lowered(file) else {
      return;
    };
    if matches!(lowered.hir.file_kind, FileKind::Dts) {
      return;
    }

    let mut literals = BTreeSet::<String>::new();
    for body_id in self.program.bodies_in_file(file) {
      let Some(body) = lowered.body(body_id) else {
        continue;
      };
      for expr in body.exprs.iter() {
        let ExprKind::Literal(Literal::String(lit)) = &expr.kind else {
          continue;
        };
        literals.insert(lit.lossy.clone());
      }
    }

    if literals.is_empty() {
      return;
    }

    let i8_ty = self.context.i8_type();
    let mut globals = BTreeMap::<String, StringLiteralGlobals<'ctx>>::new();

    for (idx, value) in literals.into_iter().enumerate() {
      let bytes = value.as_bytes();
      let bytes_len = bytes.len() as u64;

      // Constant byte storage for the literal (exactly the UTF-8 bytes of `lossy`).
      let bytes_sym = format!("__nativejs_strlit_bytes_{:08x}_{idx:04}", file.0);
      let arr_ty = i8_ty.array_type(bytes.len() as u32);
      let bytes_global = self.module.add_global(arr_ty, None, &bytes_sym);
      bytes_global.set_linkage(Linkage::Internal);
      bytes_global.set_constant(true);
      let elems: Vec<_> = bytes
        .iter()
        .copied()
        .map(|b| i8_ty.const_int(b as u64, false))
        .collect();
      bytes_global.set_initializer(&i8_ty.const_array(&elems));

      // Storage for the interned ID (filled in by the file initializer).
      let id_sym = format!("__nativejs_strlit_id_{:08x}_{idx:04}", file.0);
      let id_global = self.module.add_global(self.i32_ty, None, &id_sym);
      id_global.set_linkage(Linkage::Internal);
      // Initialize to the runtime-native invalid sentinel (`u32::MAX`) to make missing initialization
      // easier to spot in debug output/IR.
      id_global.set_initializer(&self.i32_ty.const_int(u64::from(u32::MAX), false));

      globals.insert(
        value,
        StringLiteralGlobals {
          bytes: bytes_global,
          id: id_global,
          len: bytes_len,
        },
      );
    }

    self.string_literals.insert(file, globals);
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
    cg.init_debug_subprogram_file_init();

    // Intern + pin all string literals in this file before executing any other module initializer
    // statements. String literal expressions in the checked backend lower to loads of these IDs.
    cg.codegen_string_literal_inits();

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
            cg.with_body(export_body, export_types.as_ref(), |cg| cg.codegen_expr(expr))?;
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
            (TsAbiKind::GcPtr, NativeValue::GcPtr(v)) => {
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
        TsAbiKind::String => {
          let v = value.into_int_value();
          cg.builder.build_store(slot, v).expect("store param");
          NativeValue::String(v)
        }
        TsAbiKind::Void => unreachable!("void is not a valid parameter ABI kind"),
        TsAbiKind::GcPtr => {
          let v = value.into_pointer_value();
          cg.builder.build_store(slot, v).expect("store param");
          NativeValue::GcPtr(v)
        }
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
          (TsAbiKind::String, NativeValue::String(v)) => {
            cg.builder.build_return(Some(&v)).expect("failed to build return");
          }
          (TsAbiKind::GcPtr, NativeValue::GcPtr(v)) => {
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
    if self.debug_function_attrs {
      crate::stack_walking::apply_debug_function_attrs(self.context, c_main);
    }

    let builder = self.context.create_builder();
    let bb = self.context.append_basic_block(c_main, "entry");
    builder.position_at_end(bb);

    let abi = RuntimeAbi::new(self.context, &self.module);
    let rt_thread_init = abi.get_or_declare_raw(RuntimeFn::ThreadInit);
    let rt_thread_deinit = abi.get_or_declare_raw(RuntimeFn::ThreadDeinit);
    let rt_register_shape_table = abi.get_or_declare_raw(RuntimeFn::RegisterShapeTable);
    let rt_global_root_register = abi.get_or_declare_raw(RuntimeFn::GlobalRootRegister);

    let call = builder
      .build_call(rt_thread_init, &[self.i32_ty.const_zero().into()], "rt.thread.init")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build call to rt_thread_init: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    // Register the module-local runtime shape table (if any) before module initializers run.
    //
    // The runtime rejects `len == 0`, so codegen must skip the call when no shapes were emitted.
    if let Some(shape_table) = self.shape_table {
      if shape_table.len > 0 {
        let table_ptr = shape_table.table_global.as_pointer_value();
        let len_i64 = self.i64_ty.const_int(shape_table.len as u64, false);
        let call = builder
          .build_call(
            rt_register_shape_table,
            &[table_ptr.into(), len_i64.into()],
            "rt.register_shape_table",
          )
          .map_err(|e| {
          vec![diagnostics::ice(
            ice_span,
            format!("failed to build call to rt_register_shape_table: {e}"),
          )]
        })?;
        crate::stack_walking::mark_call_notail(call);
      }
    }

    // Register global root slots for any module globals that store GC pointers. This must happen
    // after thread init and before module initializers run so that any GC that occurs after global
    // initialization can trace/relocate those references safely.
    if !self.gc_root_globals.is_empty() {
      let mut roots = self.gc_root_globals.clone();
      roots.sort_by(|a, b| a.get_name().to_string_lossy().cmp(&b.get_name().to_string_lossy()));

      for global in roots {
        let slot_ptr = global.as_pointer_value();
        let call = builder
          .build_call(rt_global_root_register, &[slot_ptr.into()], "rt.global_root_register")
          .map_err(|e| {
            vec![diagnostics::ice(
              ice_span,
              format!(
                "failed to build call to rt_global_root_register for `{}`: {e}",
                global.get_name().to_string_lossy()
              ),
            )]
          })?;
        crate::stack_walking::mark_call_notail(call);
      }
    }

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
      TsAbiKind::String => call
        .try_as_basic_value()
        .left()
        .expect("non-void TS main should return a value")
        .into_int_value(),
      TsAbiKind::GcPtr => {
        return Err(vec![diagnostics::ice(
          ice_span,
          "exported `main` must not return a GC-managed pointer value".to_string(),
        )]);
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
              "unsupported global type for native-js ABI (expected number|boolean|string|gc pointer): {}",
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
          TsAbiKind::GcPtr => self.gc_ptr_ty.into(),
          TsAbiKind::String => self.i32_ty.into(),
          TsAbiKind::Void => unreachable!(),
        };
        let global = self.module.add_global(global_ty, None, &name);
        match abi_kind {
          TsAbiKind::Number => global.set_initializer(&self.f64_ty.const_float(0.0)),
          TsAbiKind::Boolean => global.set_initializer(&self.i1_ty.const_int(0, false)),
          TsAbiKind::GcPtr => global.set_initializer(&self.gc_ptr_ty.const_null()),
          TsAbiKind::String => global.set_initializer(&self.i32_ty.const_int(u64::from(u32::MAX), false)),
          TsAbiKind::Void => unreachable!(),
        };
        if matches!(abi_kind, TsAbiKind::GcPtr) {
          self.gc_root_globals.push(global);
        }
        let is_local_to_unit = !self.exported_defs.contains(&def);
        if is_local_to_unit {
          global.set_linkage(Linkage::Internal);
        }

        if let Some(debug) = self.debug.as_mut() {
          // Omit debug metadata for GC pointers for now (we only emit `number`/`boolean` DWARF
          // types in this backend).
          if !matches!(abi_kind, TsAbiKind::GcPtr) {
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
          // Memoize the import binding so later loads/stores can reuse the resolved global slot
          // without re-traversing the export chain.
          self.globals.insert(def, slot);
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
  /// Stack of nested spans used to drive `!dbg` locations on emitted LLVM instructions.
  ///
  /// When debug info is enabled, we maintain a simple stack discipline:
  /// - `init_debug_subprogram*` seeds the stack with a "base" location for the function.
  /// - `codegen_stmt`/`codegen_expr` push a location for the node they're emitting and pop on exit.
  debug_span_stack: Vec<TextRange>,
}

struct DebugLocationGuard<'ctx, 'p, 'a> {
  cg: *mut FnCodegen<'ctx, 'p, 'a>,
  active: bool,
}

impl<'ctx, 'p, 'a> Drop for DebugLocationGuard<'ctx, 'p, 'a> {
  fn drop(&mut self) {
    if !self.active {
      return;
    }
    // SAFETY: `DebugLocationGuard` is only constructed from `&mut FnCodegen` and is dropped before
    // that mutable borrow ends (stack discipline within codegen methods).
    unsafe {
      (*self.cg).pop_debug_location();
    }
  }
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
    alloca_builder.position_at_end(entry_bb);

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
      debug_span_stack: Vec::new(),
    }
  }

  fn init_debug_subprogram(&mut self, def: DefId, sig: &TsFunctionSigKind) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };

    let (file, range) = self
      .cg
      .program
      .span_of_def(def)
      .map(|s| (s.file, s.range))
      .unwrap_or((self.file, TextRange::new(0, 0)));
    let (line, _col) = debug.line_col(self.cg.program, file, range);

    let linkage_name = crate::llvm_symbol_for_def(self.cg.program, def);
    // Prefer a friendly TS name for debuggers, but still attach the stable symbol as `linkageName`.
    let name = self
      .cg
      .program
      .def_name(def)
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| "<anonymous>".to_string());
    let is_local_to_unit = self.func.get_linkage() == Linkage::Internal;

    let sp = debug.create_subprogram(
      self.cg.program,
      file,
      &name,
      Some(&linkage_name),
      line,
      sig.ret,
      &sig.params,
      self.func,
      is_local_to_unit,
    );
    self.debug_subprogram = Some(sp);

    // Seed the location stack so nested spans can reliably restore back to a "function" location.
    let base = self
      .cg
      .program
      .span_of_def(def)
      .map(|s| s.range)
      .unwrap_or_else(|| TextRange::new(0, 0));
    self.push_debug_location(base);
  }

  fn init_debug_subprogram_file_init(&mut self) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };

    let linkage_name = crate::llvm_symbol_for_file_init(self.file);
    let (line, _col) = debug.line_col(self.cg.program, self.file, TextRange::new(0, 0));
    let sp = debug.create_subprogram(
      self.cg.program,
      self.file,
      "<module init>",
      Some(&linkage_name),
      line,
      TsAbiKind::Void,
      &[],
      self.func,
      true,
    );
    self.debug_subprogram = Some(sp);

    self.push_debug_location(TextRange::new(0, 0));
  }

  fn push_debug_location(&mut self, span: TextRange) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };
    let Some(scope) = self.debug_subprogram else {
      return;
    };

    self.debug_span_stack.push(span);
    let (line, col) = debug.line_col(self.cg.program, self.file, span);
    let loc = debug.location(self.cg.context, line, col, scope);
    self.builder.set_current_debug_location(loc);
    self.alloca_builder.set_current_debug_location(loc);
  }

  fn pop_debug_location(&mut self) {
    let Some(debug) = self.cg.debug.as_mut() else {
      return;
    };
    let Some(scope) = self.debug_subprogram else {
      return;
    };

    self.debug_span_stack.pop();
    if let Some(prev) = self.debug_span_stack.last().copied() {
      let (line, col) = debug.line_col(self.cg.program, self.file, prev);
      let loc = debug.location(self.cg.context, line, col, scope);
      self.builder.set_current_debug_location(loc);
      self.alloca_builder.set_current_debug_location(loc);
    }
  }

  fn debug_location_guard(&mut self, span: TextRange) -> DebugLocationGuard<'ctx, 'p, 'a> {
    let active = self.cg.debug.is_some() && self.debug_subprogram.is_some();
    if active {
      self.push_debug_location(span);
    }
    DebugLocationGuard {
      cg: self as *mut Self,
      active,
    }
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
    if matches!(kind, TsAbiKind::Void | TsAbiKind::GcPtr) {
      return;
    }

    let (line, col) = debug.line_col(self.cg.program, self.file, span);
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
    if matches!(kind, TsAbiKind::Void | TsAbiKind::GcPtr) {
      return;
    }

    let (line, col) = debug.line_col(self.cg.program, self.file, span);
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

  fn dbg_value(&mut self, binding: BindingId, value: NativeValue<'ctx>, span: TextRange) {
    let Some(value) = value.as_basic_value() else {
      return;
    };
    let Some(debug) = self.cg.debug.as_mut() else {
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
    let (line, col) = debug.line_col(self.cg.program, self.file, span);
    debug.insert_value(self.cg.context, &self.builder, &self.cg.module, scope, var, value, line, col);

    // `insert_value` uses the raw LLVM builder API and resets the current debug location to null;
    // restore whatever location the caller established via `debug_location_guard`.
    if let Some(restore_span) = self.debug_span_stack.last().copied() {
      let (restore_line, restore_col) = debug.line_col(self.cg.program, self.file, restore_span);
      let restore_loc = debug.location(self.cg.context, restore_line, restore_col, scope);
      self.builder.set_current_debug_location(restore_loc);
    }
  }

  fn dbg_value_locals_from_slots(&mut self, span: TextRange) {
    if !self
      .cg
      .debug
      .as_ref()
      .is_some_and(|debug| debug.optimized())
    {
      return;
    };
    // Emit `dbg.value` only for currently visible locals (per lexical scopes in `self.env`), to
    // avoid producing debug loads for out-of-scope variables.
    //
    // NOTE: Keep emission order deterministic by sorting on `NameId`. The underlying `HashMap`
    // iteration order is intentionally randomized and would otherwise produce unstable IR.
    let mut seen_names: HashSet<NameId> = HashSet::new();
    let mut visible: Vec<(NameId, BindingId)> = Vec::new();
    for scope in self.env.scopes.iter().rev() {
      for (&name, &binding) in scope.iter() {
        if seen_names.insert(name) {
          visible.push((name, binding));
        }
      }
    }
    visible.sort_by_key(|(name, _binding)| *name);

    for (_name, binding) in visible {
      if !self.debug_vars.contains_key(&binding) {
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
        TsAbiKind::String => NativeValue::String(
          self
            .builder
            .build_load(self.cg.i32_ty, slot.ptr, "dbg.load")
            .expect("failed to build dbg load")
            .into_int_value(),
        ),
        TsAbiKind::GcPtr => continue,
        TsAbiKind::Void => continue,
      };
      self.dbg_value(binding, loaded, span);
    }
  }

  fn with_body<R>(
    &mut self,
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
      TsAbiKind::GcPtr => self.cg.gc_ptr_ty.as_basic_type_enum(),
      TsAbiKind::String => self.cg.i32_ty.as_basic_type_enum(),
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
      NativeValue::String(_) => self.cg.i1_ty.const_int(1, false),
      // Objects are truthy; we treat null pointers as falsy.
      NativeValue::GcPtr(v) => self.builder.build_is_not_null(v, "truthy").expect("isnotnull"),
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

  fn declare_llvm_trap(&mut self) -> FunctionValue<'ctx> {
    if let Some(existing) = self.cg.module.get_function("llvm.trap") {
      return existing;
    }
    self
      .cg
      .module
      .add_function("llvm.trap", self.cg.context.void_type().fn_type(&[], false), None)
  }

  fn emit_trap(&mut self) -> Result<(), Vec<Diagnostic>> {
    let trap = self.declare_llvm_trap();
    let call = self.builder.build_call(trap, &[], "trap").expect("call llvm.trap");
    crate::stack_walking::mark_call_notail(call);
    self
      .builder
      .build_unreachable()
      .expect("build unreachable after trap");
    Ok(())
  }

  /// Convert a `number` (`double`) to an `i64` for array indexing, trapping on NaN or out-of-range.
  ///
  /// LLVM's `fptosi` is UB for NaN/±Infinity/out-of-range values. Since indexing expressions may be
  /// any `number` at runtime, guard the conversion so codegen never emits UB.
  fn number_to_i64_index(&mut self, v: FloatValue<'ctx>) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let is_nan = self
      .builder
      .build_float_compare(FloatPredicate::UNO, v, v, "idx.isnan")
      .expect("fcmp uno");

    // Valid `i64` range is [-2^63, 2^63-1]. Use a half-open upper bound at 2^63 so the compare is
    // representable exactly in f64.
    let min = self.cg.f64_ty.const_float(-9223372036854775808.0_f64); // -2^63
    let max_excl = self.cg.f64_ty.const_float(9223372036854775808.0_f64); // 2^63
    let lt_min = self
      .builder
      .build_float_compare(FloatPredicate::OLT, v, min, "idx.ltmin")
      .expect("fcmp olt");
    let ge_max = self
      .builder
      .build_float_compare(FloatPredicate::OGE, v, max_excl, "idx.gemax")
      .expect("fcmp oge");
    let bad1 = self.builder.build_or(is_nan, lt_min, "idx.bad1").expect("or bad1");
    let bad = self.builder.build_or(bad1, ge_max, "idx.bad").expect("or bad");

    let ok_bb = self.cg.context.append_basic_block(self.func, "idx.ok");
    let trap_bb = self.cg.context.append_basic_block(self.func, "idx.bad");
    self
      .builder
      .build_conditional_branch(bad, trap_bb, ok_bb)
      .expect("branch idx.bad");

    self.builder.position_at_end(trap_bb);
    self.emit_trap()?;

    self.builder.position_at_end(ok_bb);
    Ok(
      self
        .builder
        .build_float_to_signed_int(v, self.cg.i64_ty, "idx.i64")
        .expect("fptosi idx"),
    )
  }

  fn array_len_i64(&self, arr: PointerValue<'ctx>) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let i8_ty = self.cg.context.i8_type();
    let offset = self
      .cg
      .i64_ty
      .const_int(array_abi::RT_ARRAY_LEN_OFFSET as u64, false);
    // SAFETY: `arr` is expected to be a runtime-native array pointer allocated by `rt_alloc_array`.
    let len_ptr = unsafe {
      self
        .builder
        .build_gep(i8_ty, arr, &[offset], "arr.len.ptr")
        .expect("gep arr.len")
    };
    Ok(
      self
        .builder
        .build_load(self.cg.i64_ty, len_ptr, "arr.len")
        .expect("load arr.len")
        .into_int_value(),
    )
  }

  fn array_bounds_check(
    &mut self,
    arr: PointerValue<'ctx>,
    idx_i64: IntValue<'ctx>,
  ) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let idx_nonneg = self
      .builder
      .build_int_compare(IntPredicate::SGE, idx_i64, self.cg.i64_ty.const_zero(), "idx.nonneg")
      .expect("idx.nonneg");
    let len_i64 = self.array_len_i64(arr)?;
    let idx_lt_len = self
      .builder
      .build_int_compare(IntPredicate::ULT, idx_i64, len_i64, "idx.ltlen")
      .expect("idx.ltlen");
    let ok = self
      .builder
      .build_and(idx_nonneg, idx_lt_len, "idx.ok")
      .expect("idx.ok");

    let ok_bb = self.cg.context.append_basic_block(self.func, "arr.idx.ok");
    let trap_bb = self.cg.context.append_basic_block(self.func, "arr.idx.oob");
    self
      .builder
      .build_conditional_branch(ok, ok_bb, trap_bb)
      .expect("idx bounds branch");

    self.builder.position_at_end(trap_bb);
    self.emit_trap()?;

    self.builder.position_at_end(ok_bb);
    Ok(idx_i64)
  }

  fn array_elem_ptr(
    &self,
    arr: PointerValue<'ctx>,
    idx_i64: IntValue<'ctx>,
    elem_size_bytes: u64,
  ) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    let i8_ty = self.cg.context.i8_type();
    let data_offset = self
      .cg
      .i64_ty
      .const_int(array_abi::RT_ARRAY_DATA_OFFSET_BYTES as u64, false);
    // SAFETY: `arr` is expected to be a runtime-native array pointer allocated by `rt_alloc_array`.
    let data_ptr = unsafe {
      self
        .builder
        .build_gep(i8_ty, arr, &[data_offset], "arr.data")
        .expect("gep arr.data")
    };
    let stride = self.cg.i64_ty.const_int(elem_size_bytes, false);
    let idx_bytes = self
      .builder
      .build_int_mul(idx_i64, stride, "idx.bytes")
      .expect("idx.bytes");
    // SAFETY: Caller performs a bounds check before requesting an element pointer.
    Ok(unsafe {
      self
        .builder
        .build_gep(i8_ty, data_ptr, &[idx_bytes], "arr.elem")
        .expect("gep arr.elem")
    })
  }

  fn object_field_ptr(&self, obj: PointerValue<'ctx>, offset_bytes: u32) -> PointerValue<'ctx> {
    let i8_ty = self.cg.context.i8_type();
    let off = self.cg.i64_ty.const_int(offset_bytes as u64, false);
    // SAFETY: `obj` is expected to be a runtime-native object base pointer allocated by `rt_alloc`,
    // and `offset_bytes` is derived from a checked `types-ts-interned` layout for the object's shape.
    unsafe {
      self
        .builder
        .build_gep(i8_ty, obj, &[off], "obj.field")
        .expect("gep obj.field")
    }
  }

  fn object_field_layout(
    &mut self,
    obj_ty: tti::TypeId,
    key: &hir_js::ObjectKey,
    span: Span,
  ) -> Result<(shapes::ShapeUse<'ctx>, u32, TsAbiKind), Vec<Diagnostic>> {
    let shape = self
      .cg
      .shapes
      .ensure_shape_for_type(self.cg.context, &self.cg.module, self.cg.program, obj_ty, span)?;

    let store = self.cg.program.interned_type_store();
    let (wanted_keys, label): (Vec<tti::FieldKey>, String) = match key {
      hir_js::ObjectKey::Ident(name) => {
        let name = self
          .names
          .resolve(*name)
          .ok_or_else(|| vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error("failed to resolve object property name", span)])?;
        let name_id = store.intern_name_ref(name);
        (
          vec![tti::FieldKey::Prop(tti::PropKey::String(name_id))],
          name.to_string(),
        )
      }
      hir_js::ObjectKey::String(s) => {
        let mut keys = Vec::new();
        let name_id = store.intern_name_ref(s);
        keys.push(tti::FieldKey::Prop(tti::PropKey::String(name_id)));
        if let Some(n) = canonical_decimal_string_to_i64(s) {
          keys.push(tti::FieldKey::Prop(tti::PropKey::Number(n)));
        }
        (keys, s.clone())
      }
      hir_js::ObjectKey::Number(raw) => {
        let n = number_literal_to_i64(raw).ok_or_else(|| {
          vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            format!("unsupported numeric object property key `{raw}` (expected an integer literal)"),
            span,
          )]
        })?;
        let s = n.to_string();
        let name_id = store.intern_name_ref(&s);
        (
          vec![
            tti::FieldKey::Prop(tti::PropKey::Number(n)),
            tti::FieldKey::Prop(tti::PropKey::String(name_id)),
          ],
          s,
        )
      }
      hir_js::ObjectKey::Computed(expr) => {
        let expr = self.expr_data(*expr)?;
        match &expr.kind {
          ExprKind::Literal(Literal::String(s)) => {
            let mut keys = Vec::new();
            let name_id = store.intern_name_ref(&s.lossy);
            keys.push(tti::FieldKey::Prop(tti::PropKey::String(name_id)));
            if let Some(n) = canonical_decimal_string_to_i64(&s.lossy) {
              keys.push(tti::FieldKey::Prop(tti::PropKey::Number(n)));
            }
            (keys, s.lossy.clone())
          }
          ExprKind::Literal(Literal::Number(raw)) => {
            let n = number_literal_to_i64(raw).ok_or_else(|| {
              vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                format!("unsupported numeric object property key `{raw}` (expected an integer literal)"),
                span,
              )]
            })?;
            let s = n.to_string();
            let name_id = store.intern_name_ref(&s);
            (
              vec![
                tti::FieldKey::Prop(tti::PropKey::Number(n)),
                tti::FieldKey::Prop(tti::PropKey::String(name_id)),
              ],
              s,
            )
          }
          _ => {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
              "unsupported computed property access in this codegen subset (expected a literal string or integer key)",
              span,
            )]);
          }
        }
      }
    };

    let tti::Layout::Struct { fields, .. } = store.layout(shape.payload_layout) else {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "unsupported object payload layout for field access (expected struct layout)",
        span,
      )]);
    };

    let field = fields
      .iter()
      .find(|f| wanted_keys.iter().any(|wanted| &f.key == wanted))
      .ok_or_else(|| {
      vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
        format!("unknown object property `{label}` in native-js codegen"),
        span,
      )]
    })?;

    let offset = shape.payload_base_offset.saturating_add(field.offset);

    let kind = match store.layout(field.layout) {
      tti::Layout::Scalar { abi } => match abi {
        tti::AbiScalar::Bool => TsAbiKind::Boolean,
        tti::AbiScalar::F64 => TsAbiKind::Number,
        tti::AbiScalar::I32 | tti::AbiScalar::U32 => TsAbiKind::String,
        _ => {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "unsupported scalar field type in native-js codegen (expected boolean|number|string)",
            span,
          )]);
        }
      },
      tti::Layout::Ptr { to } => {
        if to.is_gc_tracable() {
          TsAbiKind::GcPtr
        } else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "unsupported pointer field type in native-js codegen (expected GC-managed pointer)",
            span,
          )]);
        }
      }
      _ => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          "unsupported field layout in native-js codegen (expected scalar or GC pointer)",
          span,
        )]);
      }
    };

    Ok((shape, offset, kind))
  }

  fn emit_write_barrier(
    &mut self,
    owner: PointerValue<'ctx>,
    slot: PointerValue<'ctx>,
    span: Span,
  ) -> Result<(), Vec<Diagnostic>> {
    let rt = RuntimeAbi::new(self.cg.context, &self.cg.module);
    let _ = rt.emit_runtime_call(&self.builder, RuntimeFn::WriteBarrier, &[owner.into(), slot.into()], "wb").map_err(|e| {
      vec![diagnostics::ice(span, format!("failed to emit write barrier call: {e}"))]
    })?;
    Ok(())
  }

  fn codegen_string_literal_inits(&mut self) {
    let Some(literals) = self.cg.string_literals.get(&self.file) else {
      return;
    };
    if literals.is_empty() {
      return;
    }

    let rt = crate::runtime_abi::RuntimeAbi::new(self.cg.context, &self.cg.module);
    let i64_ty = self.cg.context.i64_type();

    for globals in literals.values() {
      let len = i64_ty.const_int(globals.len, false);
      let call = rt
        .emit_runtime_call(
          &self.builder,
          crate::runtime_abi::RuntimeFn::StringIntern,
          &[globals.bytes.as_pointer_value().into(), len.into()],
          "str.intern",
        )
        .expect("failed to emit rt_string_intern call");
      let id = call
        .try_as_basic_value()
        .left()
        .expect("rt_string_intern should return an InternedId")
        .into_int_value();

      self
        .builder
        .build_store(globals.id.as_pointer_value(), id)
        .expect("failed to store interned id");

      let _ = rt
        .emit_runtime_call(
          &self.builder,
          crate::runtime_abi::RuntimeFn::StringPinInterned,
          &[id.into()],
          "str.pin",
        )
        .expect("failed to emit rt_string_pin_interned call");
    }
  }

  fn codegen_stmt(&mut self, stmt_id: StmtId) -> Result<bool, Vec<Diagnostic>> {
    let (kind, span) = {
      let stmt = self.stmt(stmt_id)?;
      (stmt.kind.clone(), Span::new(self.file, stmt.span))
    };
    let _dbg = self.debug_location_guard(span.range);
    match kind {
      StmtKind::Empty | StmtKind::Debugger => Ok(true),
      StmtKind::Expr(expr) => {
        if self.codegen_print_stmt(expr)? {
          return Ok(true);
        }
        let _ = self.codegen_expr(expr)?;
        Ok(true)
      }
      StmtKind::ExportDefaultExpr(expr) => {
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
          (TsAbiKind::String, NativeValue::String(v)) => {
            self.builder
              .build_return(Some(&v))
              .expect("failed to build return");
          }
          (TsAbiKind::GcPtr, NativeValue::GcPtr(v)) => {
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
    match value {
      NativeValue::Number(v) => self.emit_print_f64(v),
      NativeValue::String(v) => self.emit_print_interned_string(v),
      other => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!(
            "the `print` intrinsic currently only supports `number` or `string` arguments (got {got:?})",
            got = other.kind()
          ),
          Span::new(self.file, expr_span),
        )]);
      }
    }
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

  fn emit_print_interned_string(&self, value: IntValue<'ctx>) {
    let rt_print = declare_rt_print_interned_string(self.cg.context, &self.cg.module);
    let call = self
      .builder
      .build_call(rt_print, &[value.into()], "native_js_print_interned_string")
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
    let out = match expected {
      TsAbiKind::Void => {
        if lhs.kind() != TsAbiKind::Void || rhs.kind() != TsAbiKind::Void {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, `boolean && boolean`, or `gcptr && gcptr`",
            span,
          )]);
        }
        NativeValue::Void
      }
      TsAbiKind::Number => {
        let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, `boolean && boolean`, or `gcptr && gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.f64_ty, "land")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::Number(phi.as_basic_value().into_float_value())
      }
      TsAbiKind::Boolean => {
        let (NativeValue::Boolean(lhs), NativeValue::Boolean(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, `boolean && boolean`, or `gcptr && gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.i1_ty, "land")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::Boolean(phi.as_basic_value().into_int_value())
      }
      TsAbiKind::String => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          "`&&` is not supported for `string` values in native-js yet",
          span,
        )]);
      }
      TsAbiKind::GcPtr => {
        let (NativeValue::GcPtr(lhs), NativeValue::GcPtr(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`&&` currently only supports `void && void`, `number && number`, `boolean && boolean`, or `gcptr && gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.gc_ptr_ty, "land")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::GcPtr(phi.as_basic_value().into_pointer_value())
      }
    };
    // Note: `dbg_value_locals_from_slots` emits loads (non-PHI instructions). Ensure it runs *after*
    // any PHI construction in this merge block so we don't violate LLVM's "PHIs must come first"
    // verifier rule.
    self.dbg_value_locals_from_slots(span.range);
    Ok(out)
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
    let out = match expected {
      TsAbiKind::Void => {
        if lhs.kind() != TsAbiKind::Void || rhs.kind() != TsAbiKind::Void {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, `boolean || boolean`, or `gcptr || gcptr`",
            span,
          )]);
        }
        NativeValue::Void
      }
      TsAbiKind::Number => {
        let (NativeValue::Number(lhs), NativeValue::Number(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, `boolean || boolean`, or `gcptr || gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.f64_ty, "lor")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::Number(phi.as_basic_value().into_float_value())
      }
      TsAbiKind::Boolean => {
        let (NativeValue::Boolean(lhs), NativeValue::Boolean(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, `boolean || boolean`, or `gcptr || gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.i1_ty, "lor")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::Boolean(phi.as_basic_value().into_int_value())
      }
      TsAbiKind::String => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          "`||` is not supported for `string` values in native-js yet",
          span,
        )]);
      }
      TsAbiKind::GcPtr => {
        let (NativeValue::GcPtr(lhs), NativeValue::GcPtr(rhs)) = (lhs, rhs) else {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`||` currently only supports `void || void`, `number || number`, `boolean || boolean`, or `gcptr || gcptr`",
            span,
          )]);
        };
        let phi = self
          .builder
          .build_phi(self.cg.gc_ptr_ty, "lor")
          .expect("failed to build phi");
        phi.add_incoming(&[(&lhs, lhs_bb), (&rhs, rhs_end_bb)]);
        NativeValue::GcPtr(phi.as_basic_value().into_pointer_value())
      }
    };
    // Note: `dbg_value_locals_from_slots` emits loads (non-PHI instructions). Ensure it runs *after*
    // any PHI construction in this merge block so we don't violate LLVM's "PHIs must come first"
    // verifier rule.
    self.dbg_value_locals_from_slots(span.range);
    Ok(out)
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
      Ok(true)
    })();

    if needs_loop_scope {
      self.env.pop_scope();
    }

    if result.is_ok() {
      self.dbg_value_locals_from_slots(span.range);
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
            NativeValue::String(v) => {
              self
                .builder
                .build_store(global.global.as_pointer_value(), v)
                .expect("failed to build store");
            }
            NativeValue::GcPtr(v) => {
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
            NativeValue::String(v) => {
              self.builder.build_store(slot, v).expect("failed to build store");
            }
            NativeValue::GcPtr(v) => {
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
    let _dbg = self.debug_location_guard(span.range);
    match kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::Satisfies { expr: inner, .. } => {
        // Type assertions are rejected in the checked strict-subset pipeline, but the HIR codegen
        // backend is also used by tests that bypass validation. If the asserted type is a
        // GC-managed pointer, materialize a null `ptr addrspace(1)` placeholder value.
        let expected_kind = self.expr_abi_kind(expr, span)?;
        if matches!(expected_kind, TsAbiKind::GcPtr) {
          let _ = self.codegen_expr(inner)?;
          let ptr = crate::llvm::gc::gc_ptr_type(self.cg.context).const_null();
          Ok(NativeValue::GcPtr(ptr))
        } else {
          self.codegen_expr(inner)
        }
      }

      ExprKind::Literal(Literal::Number(raw)) => {
        let Some(number) = JsNumber::from_literal(&raw).map(|n| n.0) else {
          return Err(vec![codes::HIR_CODEGEN_INVALID_NUMERIC_LITERAL.error(
            format!("invalid numeric literal `{raw}`"),
            span,
          )]);
        };
        Ok(NativeValue::Number(self.cg.f64_ty.const_float(number)))
      }

      ExprKind::Literal(Literal::String(lit)) => {
        let globals = self
          .cg
          .string_literals
          .get(&self.file)
          .and_then(|m| m.get(&lit.lossy))
          .copied()
          .ok_or_else(|| {
            vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error("missing string literal global", span)]
          })?;
        let id = self
          .builder
          .build_load(self.cg.i32_ty, globals.id.as_pointer_value(), "str.lit")
          .expect("failed to load interned string id")
          .into_int_value();
        Ok(NativeValue::String(id))
      }

      ExprKind::Literal(Literal::Boolean(b)) => Ok(NativeValue::Boolean(self.cg.i1_ty.const_int(u64::from(b), false))),

      ExprKind::Array(arr) => {
        // Determine the element ABI kind from the inferred/annotated type of the array expression.
        let expr_ty = self.types.expr_type(expr).ok_or_else(|| {
          vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            "missing type information for array literal expression",
            span,
          )]
        })?;

        let mut cur_ty = expr_ty;
        let elem_ty = loop {
          match self.cg.program.interned_type_kind(cur_ty) {
            tti::TypeKind::Array { ty, .. } => break ty,
            tti::TypeKind::Tuple(elems) => break elems.first().map(|e| e.ty).unwrap_or(cur_ty),
            tti::TypeKind::Infer { constraint, .. } => {
              cur_ty = constraint.ok_or_else(|| {
                vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                  "unsupported unconstrained `infer` type for array literal",
                  span,
                )]
              })?;
            }
            tti::TypeKind::Intrinsic { ty, .. } => cur_ty = ty,
            other => {
              return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                format!("unsupported array literal type in native-js codegen: {other:?}"),
                span,
              )]);
            }
          }
        };

        let elem_kind = TsAbiKind::from_value_type_kind(&self.cg.program.type_kind(elem_ty)).ok_or_else(|| {
          vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            format!(
              "unsupported array element type in native-js codegen: {}",
              self.cg.program.display_type(elem_ty)
            ),
            span,
          )]
        })?;
        if matches!(elem_kind, TsAbiKind::Void) {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            "`void`/`undefined`/`never` array elements are not supported in native-js codegen",
            span,
          )]);
        }

        let mut len: u64 = 0;
        for el in &arr.elements {
          match el {
            ArrayElement::Expr(_) => len += 1,
            _ => {
              return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                "unsupported array literal element in native-js codegen",
                span,
              )]);
            }
          }
        }

        let elem_size_bytes: u64 = match elem_kind {
          TsAbiKind::Number => 8,
          TsAbiKind::Boolean => 1,
          TsAbiKind::String => 4,
          TsAbiKind::GcPtr => 8,
          TsAbiKind::Void => unreachable!(),
        };
        let elem_size_arg: u64 = match elem_kind {
          TsAbiKind::GcPtr => (array_abi::RT_ARRAY_ELEM_PTR_FLAG_BITS | 8) as u64,
          _ => elem_size_bytes,
        };

        let len_i64 = self.cg.i64_ty.const_int(len, false);
        let elem_size_i64 = self.cg.i64_ty.const_int(elem_size_arg, false);

        let rt = RuntimeAbi::new(self.cg.context, &self.cg.module);
        let call = rt
          .emit_runtime_call(
            &self.builder,
            RuntimeFn::AllocArray,
            &[len_i64.into(), elem_size_i64.into()],
            "arr.alloc",
          )
          .map_err(|e| vec![diagnostics::ice(span, format!("failed to emit rt_alloc_array call: {e}"))])?;
        let arr_ptr = call
          .try_as_basic_value()
          .left()
          .expect("rt_alloc_array should return a value")
          .into_pointer_value();

        let mut idx: u64 = 0;
        for el in &arr.elements {
          let ArrayElement::Expr(expr) = el else { unreachable!() };
          let value = self.codegen_expr(*expr)?;

          let idx_i64 = self.cg.i64_ty.const_int(idx, false);
          match elem_kind {
            TsAbiKind::Number => {
              let NativeValue::Number(v) = value else {
                return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                  "array element initializer type mismatch (expected `number`)",
                  span,
                )]);
              };
              let slot = self.array_elem_ptr(arr_ptr, idx_i64, elem_size_bytes)?;
              self.builder.build_store(slot, v).expect("store array init");
            }
            TsAbiKind::Boolean => {
              let NativeValue::Boolean(v) = value else {
                return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                  "array element initializer type mismatch (expected `boolean`)",
                  span,
                )]);
              };
              let slot = self.array_elem_ptr(arr_ptr, idx_i64, elem_size_bytes)?;
              self.builder.build_store(slot, v).expect("store array init");
            }
            TsAbiKind::String => {
              let NativeValue::String(v) = value else {
                return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                  "array element initializer type mismatch (expected `string`)",
                  span,
                )]);
              };
              let slot = self.array_elem_ptr(arr_ptr, idx_i64, elem_size_bytes)?;
              self.builder.build_store(slot, v).expect("store array init");
            }
            TsAbiKind::GcPtr => {
              let NativeValue::GcPtr(v) = value else {
                return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                  "array element initializer type mismatch (expected GC pointer)",
                  span,
                )]);
              };
              let slot = self.array_elem_ptr(arr_ptr, idx_i64, elem_size_bytes)?;
              self.builder.build_store(slot, v).expect("store array init");
              self.emit_write_barrier(arr_ptr, slot, span)?;
            }
            TsAbiKind::Void => unreachable!(),
          }

          idx += 1;
        }

        Ok(NativeValue::GcPtr(arr_ptr))
      }

      ExprKind::Object(obj) => {
        let expr_ty = self.types.expr_type(expr).ok_or_else(|| {
          vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            "missing type information for object literal expression",
            span,
          )]
        })?;
        let shape = self
          .cg
          .shapes
          .ensure_shape_for_type(self.cg.context, &self.cg.module, self.cg.program, expr_ty, span)?;

        let size_i64 = self.cg.i64_ty.const_int(shape.size as u64, false);
        let shape_id_i32 = self
          .builder
          .build_load(self.cg.i32_ty, shape.rt_shape_id_global.as_pointer_value(), "shape.id")
          .expect("load rt shape id")
          .into_int_value();

        let rt = RuntimeAbi::new(self.cg.context, &self.cg.module);
        let call = rt
          .emit_runtime_call(
            &self.builder,
            RuntimeFn::Alloc,
            &[size_i64.into(), shape_id_i32.into()],
            "obj.alloc",
          )
          .map_err(|e| vec![diagnostics::ice(span, format!("failed to emit rt_alloc call: {e}"))])?;
        let obj_ptr = call
          .try_as_basic_value()
          .left()
          .expect("rt_alloc should return a value")
          .into_pointer_value();

        // Initialize properties in source order.
        for prop in &obj.properties {
          let ObjectProperty::KeyValue {
            key,
            value,
            method,
            ..
          } = prop
          else {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
              "unsupported object literal property in native-js codegen",
              span,
            )]);
          };
          if *method {
            return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
              "object literal methods are not supported in native-js codegen yet",
              span,
            )]);
          }

          let (_shape, field_off, field_kind) = self.object_field_layout(expr_ty, key, span)?;

          // Evaluate the RHS before forming a derived pointer to the field slot.
          let rhs = self.codegen_expr(*value)?;
          if rhs.kind() != field_kind {
            return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              "object field initializer type mismatch",
              span,
            )]);
          }

          let slot = self.object_field_ptr(obj_ptr, field_off);
          match field_kind {
            TsAbiKind::Number => {
              let NativeValue::Number(v) = rhs else { unreachable!() };
              self.builder.build_store(slot, v).expect("store obj field");
            }
            TsAbiKind::Boolean => {
              let NativeValue::Boolean(v) = rhs else { unreachable!() };
              self.builder.build_store(slot, v).expect("store obj field");
            }
            TsAbiKind::String => {
              let NativeValue::String(v) = rhs else { unreachable!() };
              self.builder.build_store(slot, v).expect("store obj field");
            }
            TsAbiKind::GcPtr => {
              let NativeValue::GcPtr(v) = rhs else { unreachable!() };
              self.builder.build_store(slot, v).expect("store obj field");
              self.emit_write_barrier(obj_ptr, slot, span)?;
            }
            TsAbiKind::Void => unreachable!(),
          }
        }

        Ok(NativeValue::GcPtr(obj_ptr))
      }
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
              (NativeValue::String(lhs), NativeValue::String(rhs)) => {
                let pred = match op {
                  BinaryOp::StrictEquality => IntPredicate::EQ,
                  BinaryOp::StrictInequality => IntPredicate::NE,
                  _ => {
                    return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_BINARY_OP.error(
                      "string equality is only supported via `===` / `!==` in this codegen subset",
                      span,
                    )]);
                  }
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
        if member.optional {
          return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
            "optional chaining is not supported in this codegen subset",
            span,
          )]);
        }

        // Array / tuple support:
        // - `arr[i]` indexing (bounds-checked, traps on OOB)
        // - `arr.length`
        if let Some(obj_ty) = self.types.expr_type(member.object) {
          let obj_kind = self.cg.program.type_kind(obj_ty);
          if matches!(obj_kind, TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. }) {
            match &member.property {
              ObjectKey::Ident(name) if self.names.resolve(*name) == Some("length") => {
                let NativeValue::GcPtr(arr) = self.codegen_expr(member.object)? else {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                    "unsupported `.length` receiver in native-js codegen",
                    span,
                  )]);
                };
                let len_i64 = self.array_len_i64(arr)?;
                let len_f = self
                  .builder
                  .build_unsigned_int_to_float(len_i64, self.cg.f64_ty, "len")
                  .expect("uitofp len");
                return Ok(NativeValue::Number(len_f));
              }
              ObjectKey::Computed(key_expr) => {
                let NativeValue::GcPtr(arr) = self.codegen_expr(member.object)? else {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                    "unsupported array index receiver in native-js codegen",
                    span,
                  )]);
                };
                let NativeValue::Number(idx_f) = self.codegen_expr(*key_expr)? else {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                    "array index expression must have type `number` in native-js codegen",
                    span,
                  )]);
                };
                let idx_i64 = self.number_to_i64_index(idx_f)?;
                let idx_i64 = self.array_bounds_check(arr, idx_i64)?;

                let elem_kind = self.expr_abi_kind(expr, span)?;
                let elem_size_bytes = match elem_kind {
                  TsAbiKind::Number => 8,
                  TsAbiKind::Boolean => 1,
                  TsAbiKind::String => 4,
                  TsAbiKind::GcPtr => 8,
                  TsAbiKind::Void => {
                    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                      "`void` array elements are not supported in native-js codegen",
                      span,
                    )]);
                  }
                };
                let slot = self.array_elem_ptr(arr, idx_i64, elem_size_bytes)?;
                return Ok(match elem_kind {
                  TsAbiKind::Number => NativeValue::Number(
                    self
                      .builder
                      .build_load(self.cg.f64_ty, slot, "elem")
                      .expect("load elem")
                      .into_float_value(),
                  ),
                  TsAbiKind::Boolean => NativeValue::Boolean(
                    self
                      .builder
                      .build_load(self.cg.i1_ty, slot, "elem")
                      .expect("load elem")
                      .into_int_value(),
                  ),
                  TsAbiKind::String => NativeValue::String(
                    self
                      .builder
                      .build_load(self.cg.i32_ty, slot, "elem")
                      .expect("load elem")
                      .into_int_value(),
                  ),
                  TsAbiKind::GcPtr => NativeValue::GcPtr(
                    self
                      .builder
                      .build_load(self.cg.gc_ptr_ty, slot, "elem")
                      .expect("load elem")
                      .into_pointer_value(),
                  ),
                  TsAbiKind::Void => unreachable!(),
                });
              }
              _ => {
                return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                  "only array/tuple indexing (`arr[i]`) and `.length` are supported member expressions in native-js codegen",
                  span,
                )]);
              }
            }
          }

        }

        // Namespace import member access (`import * as ns from "./dep"; ns.x`).
        let object_expr = self.expr_data(member.object)?;
        let is_namespace_import = if let ExprKind::Ident(object_name) = object_expr.kind {
          let object_binding = self
            .env
            .resolve(object_name)
            .or_else(|| self.cg.resolver.for_file(self.file).resolve_expr_ident(self.body, member.object));
          match object_binding {
            Some(BindingId::Def(def)) => self.cg.namespace_import_target(def).is_some(),
            _ => false,
          }
        } else {
          false
        };

        if is_namespace_import {
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
          return match global.kind {
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
            TsAbiKind::String => Ok(NativeValue::String(
              self
                .builder
                .build_load(self.cg.i32_ty, global.global.as_pointer_value(), "ns.load")
                .expect("failed to build load")
                .into_int_value(),
            )),
            TsAbiKind::GcPtr => Ok(NativeValue::GcPtr(
              self
                .builder
                .build_load(self.cg.gc_ptr_ty, global.global.as_pointer_value(), "ns.load")
                .expect("failed to build load")
                .into_pointer_value(),
            )),
            TsAbiKind::Void => Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
              "`void` values are not supported in this context",
              span,
            )]),
          };
        }

        // Typed object field load (`obj.foo` with a constant key).
        if let Some(obj_ty) = self.types.expr_type(member.object) {
          if matches!(
            self.cg.program.type_kind(obj_ty),
            TypeKindSummary::Object | TypeKindSummary::EmptyObject
          ) {
            let NativeValue::GcPtr(obj_ptr) = self.codegen_expr(member.object)? else {
              return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
                "unsupported object field receiver in native-js codegen",
                span,
              )]);
            };

            let (_shape, field_off, field_kind) = self.object_field_layout(obj_ty, &member.property, span)?;
            let slot = self.object_field_ptr(obj_ptr, field_off);
            return Ok(match field_kind {
              TsAbiKind::Number => NativeValue::Number(
                self
                  .builder
                  .build_load(self.cg.f64_ty, slot, "obj.field")
                  .expect("load obj.field")
                  .into_float_value(),
              ),
              TsAbiKind::Boolean => NativeValue::Boolean(
                self
                  .builder
                  .build_load(self.cg.i1_ty, slot, "obj.field")
                  .expect("load obj.field")
                  .into_int_value(),
              ),
              TsAbiKind::String => NativeValue::String(
                self
                  .builder
                  .build_load(self.cg.i32_ty, slot, "obj.field")
                  .expect("load obj.field")
                  .into_int_value(),
              ),
              TsAbiKind::GcPtr => NativeValue::GcPtr(
                self
                  .builder
                  .build_load(self.cg.gc_ptr_ty, slot, "obj.field")
                  .expect("load obj.field")
                  .into_pointer_value(),
              ),
              TsAbiKind::Void => unreachable!(),
            });
          }
        }

        Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_EXPR.error(
          "unsupported member expression in native-js codegen",
          span,
        )])
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
          TsAbiKind::String => NativeValue::String(
            self
              .builder
              .build_load(self.cg.i32_ty, slot.ptr, "load")
              .expect("failed to build load")
              .into_int_value(),
          ),
          TsAbiKind::GcPtr => NativeValue::GcPtr(
            self
              .builder
              .build_load(self.cg.gc_ptr_ty, slot.ptr, "load")
              .expect("failed to build load")
              .into_pointer_value(),
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
        // Assignment to a member expression:
        // - array/tuple element assignment: `arr[i] = value`
        // - typed object field assignment: `obj.foo = value` (constant key)
        //
        // Important: do not keep derived pointers (element/field slot pointers) live across may-GC
        // calls (e.g. while evaluating the RHS). We therefore evaluate the receiver first, then the
        // RHS, then compute the slot pointer and store.
        if let Some(pat) = self.body.pats.get(target.0 as usize) {
          if let PatKind::AssignTarget(target_expr) = pat.kind {
            if let Some(target_expr_data) = self.body.exprs.get(target_expr.0 as usize) {
              if let ExprKind::Member(member) = &target_expr_data.kind {
                if member.optional {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                    "unsupported assignment target",
                    span,
                  )]);
                }

                if !matches!(op, AssignOp::Assign) {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_OP.error(
                    format!("unsupported assignment operator `{op:?}`"),
                    span,
                  )]);
                }

                let Some(obj_ty) = self.types.expr_type(member.object) else {
                  return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                    "missing type information for assignment target receiver",
                    span,
                  )]);
                };
                match self.cg.program.type_kind(obj_ty) {
                  TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. } => {
                    let ObjectKey::Computed(key_expr) = &member.property else {
                      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                        "unsupported assignment target (expected `arr[i]`)",
                        span,
                      )]);
                    };
                    let key_expr = *key_expr;

                    let NativeValue::GcPtr(arr) = self.codegen_expr(member.object)? else {
                      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                        "unsupported assignment target receiver in native-js codegen",
                        span,
                      )]);
                    };
                    let NativeValue::Number(idx_f) = self.codegen_expr(key_expr)? else {
                      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                        "array index expression must have type `number` in native-js codegen",
                        span,
                      )]);
                    };
                    let idx_i64 = self.number_to_i64_index(idx_f)?;

                    // Bounds check before evaluating the RHS (so OOB traps deterministically).
                    let idx_i64 = self.array_bounds_check(arr, idx_i64)?;

                    // Evaluate RHS after bounds check (may allocate / GC).
                    let rhs = self.codegen_expr(value)?;

                    // Determine element layout from the member-expression type.
                    let elem_kind = self.expr_abi_kind(target_expr, span)?;
                    if rhs.kind() != elem_kind {
                      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                        "array element assignment type mismatch",
                        span,
                      )]);
                    }
                    let elem_size_bytes = match elem_kind {
                      TsAbiKind::Number => 8,
                      TsAbiKind::Boolean => 1,
                      TsAbiKind::String => 4,
                      TsAbiKind::GcPtr => 8,
                      TsAbiKind::Void => {
                        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                          "`void` array elements are not supported in native-js codegen",
                          span,
                        )]);
                      }
                    };

                    match elem_kind {
                      TsAbiKind::Number => {
                        let NativeValue::Number(rhs_f) = rhs else { unreachable!() };
                        let slot = self.array_elem_ptr(arr, idx_i64, elem_size_bytes)?;
                        self.builder.build_store(slot, rhs_f).expect("store array elem");
                      }
                      TsAbiKind::Boolean => {
                        let NativeValue::Boolean(rhs_b) = rhs else { unreachable!() };
                        let slot = self.array_elem_ptr(arr, idx_i64, elem_size_bytes)?;
                        self.builder.build_store(slot, rhs_b).expect("store array elem");
                      }
                      TsAbiKind::String => {
                        let NativeValue::String(rhs_s) = rhs else { unreachable!() };
                        let slot = self.array_elem_ptr(arr, idx_i64, elem_size_bytes)?;
                        self.builder.build_store(slot, rhs_s).expect("store array elem");
                      }
                      TsAbiKind::GcPtr => {
                        let NativeValue::GcPtr(rhs_p) = rhs else { unreachable!() };
                        let slot = self.array_elem_ptr(arr, idx_i64, elem_size_bytes)?;
                        self.builder.build_store(slot, rhs_p).expect("store array elem");
                        self.emit_write_barrier(arr, slot, span)?;
                      }
                      TsAbiKind::Void => unreachable!(),
                    }

                    return Ok(rhs);
                  }
                  TypeKindSummary::Object | TypeKindSummary::EmptyObject => {
                    let NativeValue::GcPtr(obj_ptr) = self.codegen_expr(member.object)? else {
                      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET.error(
                        "unsupported assignment target receiver in native-js codegen",
                        span,
                      )]);
                    };

                    let (_shape, field_off, field_kind) = self.object_field_layout(obj_ty, &member.property, span)?;

                    let rhs = self.codegen_expr(value)?;
                    if rhs.kind() != field_kind {
                      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
                        "object field assignment type mismatch",
                        span,
                      )]);
                    }

                    let slot = self.object_field_ptr(obj_ptr, field_off);
                    match field_kind {
                      TsAbiKind::Number => {
                        let NativeValue::Number(rhs_f) = rhs else { unreachable!() };
                        self.builder.build_store(slot, rhs_f).expect("store obj field");
                      }
                      TsAbiKind::Boolean => {
                        let NativeValue::Boolean(rhs_b) = rhs else { unreachable!() };
                        self.builder.build_store(slot, rhs_b).expect("store obj field");
                      }
                      TsAbiKind::String => {
                        let NativeValue::String(rhs_s) = rhs else { unreachable!() };
                        self.builder.build_store(slot, rhs_s).expect("store obj field");
                      }
                      TsAbiKind::GcPtr => {
                        let NativeValue::GcPtr(rhs_p) = rhs else { unreachable!() };
                        self.builder.build_store(slot, rhs_p).expect("store obj field");
                        self.emit_write_barrier(obj_ptr, slot, span)?;
                      }
                      TsAbiKind::Void => unreachable!(),
                    }

                    return Ok(rhs);
                  }
                  _ => {
                    // Fall through to the non-member assignment code path.
                  }
                }
              }
            }
          }
        }

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
          TsAbiKind::String => Ok(NativeValue::String(
            call
              .try_as_basic_value()
              .left()
              .expect("non-void call should return a value")
              .into_int_value(),
          )),
          TsAbiKind::GcPtr => Ok(NativeValue::GcPtr(
            call
              .try_as_basic_value()
              .left()
              .expect("non-void call should return a value")
              .into_pointer_value(),
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
      NativeValue::String(v) => {
        self
          .builder
          .build_store(slot.ptr, v)
          .expect("failed to build store");
      }
      NativeValue::GcPtr(v) => {
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

fn declare_rt_print_interned_string<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("rt_print_interned_string") {
    return existing;
  }

  let void_ty = context.void_type();
  let i32_ty = context.i32_type();
  let i64_ty = context.i64_type();
  let ptr_ty = context.ptr_type(AddressSpace::default());

  let func = module.add_function(
    "rt_print_interned_string",
    void_ty.fn_type(&[i32_ty.into()], false),
    Some(Linkage::Internal),
  );
  // Keep frame pointers / disable tail calls for stack walking, but do not mark
  // this helper as GC-managed. It must not contain statepoints/stackmaps.
  crate::stack_walking::apply_stack_walking_frame_attrs(context, func);

  let builder = context.create_builder();
  let bb = context.append_basic_block(func, "entry");
  builder.position_at_end(bb);

  // Runtime ABI: `bool rt_string_lookup_pinned(InternedId id, StringRef* out)`.
  // We only use the pinned lookup because it returns a GC-stable pointer (stored out-of-line in the
  // interner). This makes it safe to pass through normal syscalls/stdio without holding GC handles.
  let lookup = declare_rt_string_lookup_pinned(context, module);

  // `StringRef` layout is `{ ptr: *const u8, len: usize }` (runtime-native ABI is 64-bit only).
  let string_ref_ty = context.struct_type(&[ptr_ty.into(), i64_ty.into()], false);
  let out = builder
    .build_alloca(string_ref_ty, "strref.out")
    .expect("alloca StringRef");

  let id = func
    .get_nth_param(0)
    .expect("missing interned string id")
    .into_int_value();

  let ok = builder
    .build_call(lookup, &[id.into(), out.into()], "lookup_pinned")
    .expect("call rt_string_lookup_pinned")
    .try_as_basic_value()
    .left()
    .expect("rt_string_lookup_pinned should return a value")
    .into_int_value();

  let ok_bb = context.append_basic_block(func, "ok");
  let fail_bb = context.append_basic_block(func, "fail");
  let end_bb = context.append_basic_block(func, "end");
  builder
    .build_conditional_branch(ok, ok_bb, fail_bb)
    .expect("branch on lookup_pinned result");

  // Success path: write raw bytes + newline to stdout (fd=1).
  builder.position_at_end(ok_bb);
  let write = declare_write(context, module);
  let fd_stdout = i32_ty.const_int(1, false);

  let ptr_gep = builder
    .build_struct_gep(string_ref_ty, out, 0, "ptr.gep")
    .expect("gep ptr");
  let len_gep = builder
    .build_struct_gep(string_ref_ty, out, 1, "len.gep")
    .expect("gep len");

  let bytes_ptr = builder
    .build_load(ptr_ty, ptr_gep, "bytes.ptr")
    .expect("load bytes ptr")
    .into_pointer_value();
  let bytes_len = builder
    .build_load(i64_ty, len_gep, "bytes.len")
    .expect("load bytes len")
    .into_int_value();

  let call = builder
    .build_call(write, &[fd_stdout.into(), bytes_ptr.into(), bytes_len.into()], "write.str")
    .expect("call write(str)");
  crate::stack_walking::mark_call_notail(call);

  let nl = builder
    .build_global_string_ptr("\n", "native_js_print_str_nl")
    .expect("create newline string");
  let one = i64_ty.const_int(1, false);
  let call = builder
    .build_call(
      write,
      &[fd_stdout.into(), nl.as_pointer_value().into(), one.into()],
      "write.nl",
    )
    .expect("call write(nl)");
  crate::stack_walking::mark_call_notail(call);

  builder
    .build_unconditional_branch(end_bb)
    .expect("branch to end");

  // Failure path: fall back to printing the numeric interned id (`u32`).
  builder.position_at_end(fail_bb);
  let printf = declare_printf(context, module);
  let fmt = builder
    .build_global_string_ptr("%u\n", "native_js_print_interned_fallback_fmt")
    .expect("failed to create printf format string");
  let call = builder
    .build_call(
      printf,
      &[fmt.as_pointer_value().into(), id.into()],
      "native_js_print_interned_fallback",
    )
    .expect("failed to build printf call");
  crate::stack_walking::mark_call_notail(call);
  builder
    .build_unconditional_branch(end_bb)
    .expect("branch to end");

  builder.position_at_end(end_bb);
  builder.build_return(None).expect("failed to build return");

  func
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

fn declare_write<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("write") {
    return existing;
  }
  let i32_ty = context.i32_type();
  let i64_ty = context.i64_type();
  let ptr_ty = context.ptr_type(AddressSpace::default());
  module.add_function("write", i64_ty.fn_type(&[i32_ty.into(), ptr_ty.into(), i64_ty.into()], false), None)
}

fn declare_rt_string_lookup_pinned<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("rt_string_lookup_pinned") {
    return existing;
  }
  let i1_ty = context.bool_type();
  let i32_ty = context.i32_type();
  let ptr_ty = context.ptr_type(AddressSpace::default());
  module.add_function("rt_string_lookup_pinned", i1_ty.fn_type(&[i32_ty.into(), ptr_ty.into()], false), None)
}

mod builtins;
pub mod safepoint;
pub(crate) mod llvm;

use crate::CompileOptions;
use parse_js::ast::node::Node;
use parse_js::ast::stx::TopLevel;
use parse_js::loc::Loc;

#[derive(thiserror::Error, Debug)]
pub enum CodegenError {
  #[error("unsupported statement")]
  UnsupportedStmt { loc: Loc },

  #[error("unsupported expression")]
  UnsupportedExpr { loc: Loc },

  #[error("unsupported operator: {op:?}")]
  UnsupportedOperator { op: parse_js::operator::OperatorName, loc: Loc },

  #[error("builtins disabled")]
  BuiltinsDisabled { loc: Loc },

  #[error("type error: {message}")]
  TypeError { message: String, loc: Loc },
}

pub fn emit_llvm_module(
  ast: &Node<TopLevel>,
  source: &str,
  opts: CompileOptions,
) -> Result<String, CodegenError> {
  llvm::emit_llvm_module(ast, source, opts)
}
