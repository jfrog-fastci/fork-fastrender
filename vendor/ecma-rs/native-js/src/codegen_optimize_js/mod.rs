//! Optimize-js driven native backend.
//!
//! This backend consumes `optimize-js`'s SSA CFG (`keep_ssa=true`) together with
//! `typecheck-ts` layout metadata (`InstMeta.native_layout`) and lowers it to
//! `runtime-native` ABI calls + direct offset-based field access.
//!
//! This is intentionally a **strict** subset for now:
//! - GC-managed objects allocated via `rt_alloc` using a generated shape table.
//! - GC-managed arrays via `rt_alloc_array`.
//! - Constant-key property access lowered to direct loads/stores.
//! - No dynamic property access and no layouts requiring tag-dispatch tracing.

use crate::codes;
use crate::runtime_abi::{RuntimeAbi, RuntimeCallError};
use crate::runtime_fn::RuntimeFn;
use crate::strict::Entrypoint;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{DefKind, FileKind, StmtKind};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicTypeEnum, FloatType, IntType};
use inkwell::values::{BasicValueEnum, FunctionValue, GlobalValue, IntValue, PhiValue, PointerValue};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::symbol::semantics::SymbolId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use typecheck_ts::{DefId, FileId, Program, TypeKindSummary};
use types_ts_interned::{AbiScalar, ArrayElemRepr, FieldKey, GcTraceKind, Layout, LayoutId, PtrKind};

/// Entry point for the optimize-js based backend.
pub fn codegen<'ctx>(
  context: &'ctx Context,
  program: &Program,
  entry_file: FileId,
  entrypoint: Entrypoint,
  options: crate::codegen::CodegenOptions,
) -> Result<Module<'ctx>, Vec<Diagnostic>> {
  let mut cg = OptimizeJsCodegen::new(context, program, entry_file, entrypoint, &options);
  cg.compile()?;
  Ok(cg.module)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TsAbiKind {
  Number,
  Boolean,
  Void,
  /// `types-ts-interned` represents `string` as an `InternedId` (`u32`).
  String,
  GcPtr,
}

impl TsAbiKind {
  fn from_return_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(Self::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(Self::Boolean),
      TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never => Some(Self::Void),
      TypeKindSummary::String | TypeKindSummary::StringLiteral(_) | TypeKindSummary::TemplateLiteral => Some(Self::String),
      TypeKindSummary::EmptyObject | TypeKindSummary::Object => Some(Self::GcPtr),
      TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. } => Some(Self::GcPtr),
      _ => None,
    }
  }
}

#[derive(Clone, Debug)]
struct FnSig {
  param_layouts: Vec<LayoutId>,
  ret_kind: TsAbiKind,
}

#[derive(Clone, Debug)]
struct ShapeInfo {
  shape_id_raw: u32,
  payload_base_offset: u32,
  object_size: u32,
  object_align: u32,
  ptr_offsets: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
struct ShapeTable {
  /// Shapes in `RtShapeId` order (1-indexed).
  shapes: Vec<ShapeInfo>,
  /// Lookup payload layout -> shape index in `shapes`.
  by_payload: HashMap<LayoutId, usize>,
}

impl ShapeTable {
  fn len(&self) -> usize {
    self.shapes.len()
  }

  fn get_by_payload(&self, payload: LayoutId) -> Option<&ShapeInfo> {
    self.by_payload.get(&payload).map(|&idx| &self.shapes[idx])
  }
}

struct OptimizeJsCodegen<'ctx, 'p> {
  context: &'ctx Context,
  module: Module<'ctx>,
  builder: Builder<'ctx>,
  program: &'p Program,
  entry_file: FileId,
  entrypoint: Entrypoint,
  options: &'p crate::codegen::CodegenOptions,

  // Cached LLVM types.
  i1: IntType<'ctx>,
  i8: IntType<'ctx>,
  i16: IntType<'ctx>,
  i32: IntType<'ctx>,
  i64: IntType<'ctx>,
  f64: FloatType<'ctx>,

  // Pointer types (opaque pointer mode).
  ptr_raw: inkwell::types::PointerType<'ctx>,
  ptr_gc: inkwell::types::PointerType<'ctx>,

  // String literal pool (bytes globals).
  next_string_global_id: u32,
  string_bytes_globals: HashMap<String, GlobalValue<'ctx>>,
}

impl<'ctx, 'p> OptimizeJsCodegen<'ctx, 'p> {
  fn new(
    context: &'ctx Context,
    program: &'p Program,
    entry_file: FileId,
    entrypoint: Entrypoint,
    options: &'p crate::codegen::CodegenOptions,
  ) -> Self {
    let module = context.create_module(&options.module_name);
    let builder = context.create_builder();
    let i1 = context.bool_type();
    let i8 = context.i8_type();
    let i16 = context.i16_type();
    let i32 = context.i32_type();
    let i64 = context.i64_type();
    let f64 = context.f64_type();
    let ptr_raw = context.ptr_type(AddressSpace::default());
    let ptr_gc = context.ptr_type(crate::llvm::gc::gc_address_space());

    Self {
      context,
      module,
      builder,
      program,
      entry_file,
      entrypoint,
      options,
      i1,
      i8,
      i16,
      i32,
      i64,
      f64,
      ptr_raw,
      ptr_gc,
      next_string_global_id: 0,
      string_bytes_globals: HashMap::new(),
    }
  }

  fn compile(&mut self) -> Result<(), Vec<Diagnostic>> {
    // Compile optimize-js IR for the entry file. Use `run_opt_passes=false` so object/array
    // allocation marker calls survive for lowering (scalar replacement can eliminate them).
    let native_ready = optimize_js::compile_file_native_ready_programless(
      self.program,
      self.entry_file,
      optimize_js::TopLevelMode::Module,
      self.options.debug,
      optimize_js::NativeReadyOptions {
        run_opt_passes: false,
        ..Default::default()
      },
    )?;

    // MVP limitation: compile a single file/module at a time (no imports/re-exports).
    if self.entrypoint.main_def.file() != self.entry_file {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "optimize-js backend currently requires `main` to be defined in the entry file (imports/re-exports not supported yet)",
        Span::new(self.entry_file, TextRange::new(0, 0)),
      )]);
    }

    // Map optimize-js FnId order to TypeScript DefIds by replicating optimize-js's hoisting order.
    // This keeps function signatures deterministic and allows direct calls (`Arg::Fn`) to be
    // lowered as LLVM direct calls.
    let fn_defs = self.collect_hoisted_function_defs(self.entry_file)?;
    if fn_defs.len() != native_ready.program.functions.len() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        format!(
          "optimize-js backend expected {} hoisted functions but optimize-js produced {} (nested functions are not supported yet)",
          fn_defs.len(),
          native_ready.program.functions.len()
        ),
        Span::new(self.entry_file, TextRange::new(0, 0)),
      )]);
    }

    let main_fn_idx = fn_defs
      .iter()
      .position(|def| *def == self.entrypoint.main_def)
      .ok_or_else(|| {
        vec![codes::HIR_CODEGEN_MISSING_FUNCTION_HIR.error(
          "failed to map exported `main` to an optimize-js function id",
          Span::new(self.entry_file, TextRange::new(0, 0)),
        )]
      })?;

    // Resolve native ABI signatures for all functions.
    let mut fn_sigs = Vec::<FnSig>::with_capacity(fn_defs.len());
    for (idx, def) in fn_defs.iter().copied().enumerate() {
      let expected_param_count = native_ready.program.functions[idx].params.len();
      fn_sigs.push(self.ts_function_sig(def, expected_param_count)?);
    }

    let main_ret = fn_sigs[main_fn_idx].ret_kind;

    // Build deterministic shape table for all GC object payload layouts used by rt_alloc.
    let shape_table = self.build_shape_table(&native_ready.program)?;
    let shape_table_global = self.emit_shape_table_globals(&shape_table);

    // Declare core runtime ABI symbols/wrappers.
    RuntimeAbi::new(self.context, &self.module).declare_all();

    // Collect foreign symbol -> function id mapping from the top-level body. This allows nested
    // functions to `ForeignLoad` a hoisted function declaration and still be compiled as a direct
    // call.
    let foreign_fn_map = collect_foreign_fn_map(native_ready.program.top_level.analyzed_cfg());

    // Declare all functions up front so we can emit direct calls.
    let mut llvm_fns: Vec<FunctionValue<'ctx>> = Vec::with_capacity(native_ready.program.functions.len());
    for (idx, def) in fn_defs.iter().copied().enumerate() {
      let sig = &fn_sigs[idx];

      let mut param_tys: Vec<BasicTypeEnum<'ctx>> = Vec::with_capacity(sig.param_layouts.len());
      for &layout in sig.param_layouts.iter() {
        param_tys.push(llvm_type_for_layout(
          self.program,
          layout,
          self.context,
          self.ptr_gc,
          self.f64,
          self.i1,
          self.i64,
          self.i32,
          self.i8,
        )?);
      }
      let param_tys_meta: Vec<_> = param_tys.iter().map(|t| (*t).into()).collect();

      let fn_ty = match sig.ret_kind {
        TsAbiKind::Void => self.context.void_type().fn_type(&param_tys_meta, false),
        TsAbiKind::Number => self.f64.fn_type(&param_tys_meta, false),
        TsAbiKind::Boolean => self.i1.fn_type(&param_tys_meta, false),
        TsAbiKind::String => self.i32.fn_type(&param_tys_meta, false),
        TsAbiKind::GcPtr => self.ptr_gc.fn_type(&param_tys_meta, false),
      };

      let name = if idx == main_fn_idx {
        "__nativejs_ts_main".to_string()
      } else {
        crate::llvm_symbol_for_def(self.program, def)
      };
      let linkage = if idx == main_fn_idx { Some(Linkage::Internal) } else { Some(Linkage::Internal) };
      let f = self.module.add_function(&name, fn_ty, linkage);
      crate::stack_walking::apply_stack_walking_attrs(self.context, f);
      llvm_fns.push(f);
    }

    // Lower optimize-js IL for all functions.
    for (idx, func) in native_ready.program.functions.iter().enumerate() {
      self.codegen_optimize_js_function(
        func,
        llvm_fns[idx],
        &fn_sigs[idx],
        &llvm_fns,
        &fn_sigs,
        &foreign_fn_map,
        &shape_table,
      )?;
    }

    // Define C `main` wrapper and register shape table.
    let ts_main = llvm_fns[main_fn_idx];
    self.build_c_main(ts_main, main_ret, shape_table_global, shape_table.len())?;

    Ok(())
  }

  fn collect_hoisted_function_defs(&self, file: FileId) -> Result<Vec<DefId>, Vec<Diagnostic>> {
    let lowered = self.program.hir_lowered(file).ok_or_else(|| {
      vec![codes::MISSING_ENTRY_HIR.error(
        "failed to access lowered HIR for entry file",
        Span::new(file, TextRange::new(0, 0)),
      )]
    })?;
    if matches!(lowered.hir.file_kind, FileKind::Dts) {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "entry file must not be a declaration file",
        Span::new(file, TextRange::new(0, 0)),
      )]);
    }

    let root = lowered.body(lowered.root_body()).ok_or_else(|| {
      vec![codes::HIR_CODEGEN_MISSING_FUNCTION_HIR.error(
        "failed to access lowered root body for entry file",
        Span::new(file, TextRange::new(0, 0)),
      )]
    })?;

    // Mirror optimize-js's deterministic hoisting order: sort decl statements by span.
    let mut decls = Vec::new();
    for stmt in root.stmts.iter() {
      if let StmtKind::Decl(def_id) = stmt.kind {
        decls.push((stmt.span.start, stmt.span.end, def_id));
      }
    }
    decls.sort_by_key(|(start, end, def_id)| (*start, *end, *def_id));

    let mut out = Vec::new();
    for (_, _, def_id) in decls {
      let Some(def) = lowered.def(def_id) else {
        continue;
      };
      if def.path.kind != DefKind::Function {
        continue;
      }
      // Skip overloads/ambient declarations (no runtime body), matching optimize-js.
      if def.body.is_none() {
        continue;
      }
      out.push(def_id);
    }
    Ok(out)
  }

  fn ts_function_sig(&self, def: DefId, expected_param_count: usize) -> Result<FnSig, Vec<Diagnostic>> {
    let span = self
      .program
      .span_of_def(def)
      .unwrap_or_else(|| Span::new(def.file(), TextRange::new(0, 0)));

    let func_ty = self.program.type_of_def_interned(def);
    let sigs = self.program.call_signatures(func_ty);
    if sigs.is_empty() {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "failed to resolve call signature for function",
        span,
      )]);
    }
    if sigs.len() != 1 {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "only single-signature functions are supported by the optimize-js backend right now",
        span,
      )]);
    }
    let sig = &sigs[0].signature;
    if sig.this_param.is_some() {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "`this` parameters are not supported by the optimize-js backend",
        span,
      )]);
    }
    if !sig.type_params.is_empty() {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "generic functions are not supported by the optimize-js backend",
        span,
      )]);
    }
    if sig.params.len() != expected_param_count {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        "native-js: signature/parameter count mismatch",
        span,
      )]);
    }

    let mut param_layouts = Vec::with_capacity(sig.params.len());
    for (idx, param) in sig.params.iter().enumerate() {
      if param.optional || param.rest {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!("optional/rest parameters are not supported by native-js yet (param #{idx})"),
          span,
        )]);
      }

      // Validate the ABI type set supported by this backend. (Even if `layout_of_interned`
      // produces a layout, we want stable and user-friendly diagnostics here.)
      let kind = self.program.type_kind(param.ty);
      match kind {
        TypeKindSummary::Number
        | TypeKindSummary::NumberLiteral(_)
        | TypeKindSummary::Boolean
        | TypeKindSummary::BooleanLiteral(_)
        | TypeKindSummary::String
        | TypeKindSummary::StringLiteral(_)
        | TypeKindSummary::TemplateLiteral
        | TypeKindSummary::EmptyObject
        | TypeKindSummary::Object
        | TypeKindSummary::Array { .. }
        | TypeKindSummary::Tuple { .. } => {}
        _ => {
          return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
            format!(
              "unsupported parameter type for optimize-js backend ABI: {}",
              self.program.display_type(param.ty)
            ),
            span,
          )]);
        }
      }
      param_layouts.push(self.program.layout_of_interned(param.ty));
    }

    let ret_kind = self.program.type_kind(sig.ret);
    let ret_kind = TsAbiKind::from_return_type_kind(&ret_kind).ok_or_else(|| {
      vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported return type for optimize-js backend: {}",
          self.program.display_type(sig.ret)
        ),
        span,
      )]
    })?;

    Ok(FnSig {
      param_layouts,
      ret_kind,
    })
  }

  fn build_c_main(
    &mut self,
    ts_main: FunctionValue<'ctx>,
    ts_main_ret: TsAbiKind,
    shape_table_global: Option<GlobalValue<'ctx>>,
    shape_table_len: usize,
  ) -> Result<(), Vec<Diagnostic>> {
    let ice_span = Span::new(self.entry_file, TextRange::new(0, 0));

    let c_main = self.module.add_function("main", self.i32.fn_type(&[], false), None);
    crate::stack_walking::apply_stack_walking_attrs(self.context, c_main);

    let bb = self.context.append_basic_block(c_main, "entry");
    self.builder.position_at_end(bb);

    let abi = RuntimeAbi::new(self.context, &self.module);
    let rt_thread_init = abi.get_or_declare_raw(RuntimeFn::ThreadInit);
    let rt_thread_deinit = abi.get_or_declare_raw(RuntimeFn::ThreadDeinit);
    let rt_register_shape_table = abi.get_or_declare_raw(RuntimeFn::RegisterShapeTable);

    let call = self
      .builder
      .build_call(rt_thread_init, &[self.i32.const_zero().into()], "rt.thread.init")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build rt_thread_init: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    if shape_table_len > 0 {
      let table = shape_table_global.expect("shape_table_len > 0 implies global exists");
      let ptr = table.as_pointer_value();
      let len = self.i64.const_int(shape_table_len as u64, false);
      let call = self.builder.build_call(
        rt_register_shape_table,
        &[ptr.into(), len.into()],
        "rt.register.shape_table",
      ).map_err(|e| vec![diagnostics::ice(
        ice_span,
        format!("failed to build rt_register_shape_table: {e}"),
      )])?;
      crate::stack_walking::mark_call_notail(call);
    }

    let call = self
      .builder
      .build_call(ts_main, &[], "ts.main")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build call to ts main: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    let ret_val = match ts_main_ret {
      TsAbiKind::Void => self.i32.const_zero(),
      TsAbiKind::Number => {
        let v = call
          .try_as_basic_value()
          .left()
          .expect("ts main should return a value")
          .into_float_value();
        self
          .builder
          .build_float_to_signed_int(v, self.i32, "exitcode")
          .expect("fptosi")
      }
      TsAbiKind::Boolean => {
        let v = call
          .try_as_basic_value()
          .left()
          .expect("ts main should return a value")
          .into_int_value();
        self
          .builder
          .build_int_z_extend(v, self.i32, "exitcode")
          .expect("zext")
      }
      TsAbiKind::String => {
        // Returning an interned string id from `main` is not meaningful; treat as success.
        self.i32.const_zero()
      }
      TsAbiKind::GcPtr => {
        // Returning a pointer from `main` is not meaningful; treat as success.
        self.i32.const_zero()
      }
    };

    let call = self
      .builder
      .build_call(rt_thread_deinit, &[], "rt.thread.deinit")
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build rt_thread_deinit: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);

    self
      .builder
      .build_return(Some(&ret_val))
      .map_err(|e| vec![diagnostics::ice(ice_span, format!("failed to build return in C main wrapper: {e}"))])?;
    Ok(())
  }

  fn build_shape_table(
    &self,
    oj: &optimize_js::Program,
  ) -> Result<ShapeTable, Vec<Diagnostic>> {
    let store = self.program.interned_type_store();

    // Collect payload layouts keyed by LayoutId (stable sort order) and remember the first span for
    // diagnostics.
    let mut payloads: BTreeMap<LayoutId, TextRange> = BTreeMap::new();

    // Collect shapes for all functions we codegen. (We do not currently codegen top-level module
    // initialization.)
    for f in oj.functions.iter() {
      collect_object_payloads_from_cfg(&store, oj.source_file, f.analyzed_cfg(), &mut payloads)?;
    }

    let mut out = ShapeTable::default();
    for (idx, (payload, span)) in payloads.into_iter().enumerate() {
      let shape_id_raw = (idx as u32) + 1;

      let kind = store.layout_gc_trace_kind(payload);
      if kind == GcTraceKind::RequiresTagDispatch {
        return Err(vec![codes::OPTIMIZE_JS_UNTRACEABLE_GC_LAYOUT.error(
          "GC object layout requires conditional tracing (tag-dispatch) and is not supported yet",
          Span::new(oj.source_file, span),
        )
        .with_note(
          "this layout contains a tagged union whose pointer slots vary by variant; \
runtime-native shape descriptors can currently only express flat pointer maps",
        )]);
      }

      let payload_layout = store.layout(payload);
      let payload_size = payload_layout.size();
      let payload_align = payload_layout.align();

      let payload_base_offset = align_up(
        core::mem::size_of::<runtime_native_abi::RtGcPrefix>() as u32,
        payload_align,
      );
      let object_align = payload_align.max(runtime_native_abi::RT_PTR_ALIGN_BYTES as u32);
      let object_size = align_up(payload_base_offset + payload_size, object_align);

      let mut ptr_offsets = store.gc_ptr_offsets(payload);
      for off in ptr_offsets.iter_mut() {
        *off = off.saturating_add(payload_base_offset);
      }

      let info = ShapeInfo {
        shape_id_raw,
        payload_base_offset,
        object_size,
        object_align,
        ptr_offsets,
      };
      out.by_payload.insert(payload, out.shapes.len());
      out.shapes.push(info);
    }

    Ok(out)
  }

  fn emit_shape_table_globals(&mut self, shapes: &ShapeTable) -> Option<GlobalValue<'ctx>> {
    if shapes.shapes.is_empty() {
      return None;
    }

    let desc_ty = self.context.struct_type(
      &[
        self.i32.into(),    // size: u32
        self.i16.into(),    // align: u16
        self.i16.into(),    // flags: u16
        self.ptr_raw.into(), // ptr_offsets: *const u32
        self.i32.into(),    // ptr_offsets_len: u32
        self.i32.into(),    // reserved: u32
      ],
      false,
    );

    let mut desc_values = Vec::with_capacity(shapes.shapes.len());

    for shape in &shapes.shapes {
      let offsets_ptr = if shape.ptr_offsets.is_empty() {
        self.ptr_raw.const_null()
      } else {
        let arr_ty = self.i32.array_type(shape.ptr_offsets.len() as u32);
        let values: Vec<_> = shape
          .ptr_offsets
          .iter()
          .map(|off| self.i32.const_int(*off as u64, false))
          .collect();
        let init = self.i32.const_array(&values);
        let gv = self.module.add_global(arr_ty, None, &format!("__nativejs_ptr_offsets_{}", shape.shape_id_raw));
        gv.set_linkage(Linkage::Internal);
        gv.set_constant(true);
        gv.set_initializer(&init);
        gv.as_pointer_value()
      };

      let fields = &[
        self.i32.const_int(shape.object_size as u64, false).into(),
        self.i16.const_int(shape.object_align as u64, false).into(),
        self.i16.const_int(0, false).into(), // flags
        offsets_ptr.into(),
        self
          .i32
          .const_int(shape.ptr_offsets.len() as u64, false)
          .into(),
        self.i32.const_int(0, false).into(), // reserved
      ];
      let desc = desc_ty.const_named_struct(fields);
      desc_values.push(desc);
    }

    let table_ty = desc_ty.array_type(desc_values.len() as u32);
    let table_init = desc_ty.const_array(&desc_values);
    let table = self
      .module
      .add_global(table_ty, None, "__nativejs_shape_table");
    table.set_linkage(Linkage::Internal);
    table.set_constant(true);
    table.set_initializer(&table_init);
    Some(table)
  }

  fn codegen_optimize_js_function(
    &mut self,
    func: &optimize_js::ProgramFunction,
    llvm_func: FunctionValue<'ctx>,
    sig: &FnSig,
    llvm_fns: &[FunctionValue<'ctx>],
    fn_sigs: &[FnSig],
    foreign_fn_map: &HashMap<SymbolId, usize>,
    shapes: &ShapeTable,
  ) -> Result<(), Vec<Diagnostic>> {
    let cfg = func.analyzed_cfg();
    let ret_kind = sig.ret_kind;

    if std::env::var_os("NATIVE_JS_DUMP_OPTIMIZE_JS_CFG").is_some() {
      eprintln!("=== native-js optimize-js CFG dump ===");
      eprintln!("llvm func: {}", llvm_func.get_name().to_string_lossy());
      eprintln!("optimize-js params: {:?}", func.params);
      eprintln!("entry: b{}", cfg.entry);
      let mut labels: Vec<u32> = cfg.bblocks.all().map(|(l, _)| l).collect();
      labels.sort_unstable();
      for label in labels {
        eprintln!("b{label}:");
        for inst in cfg.bblocks.get(label).iter() {
          eprintln!("  {inst:?}");
        }
        eprintln!("  terminator: {:?}", cfg.terminator(label));
      }
    }

    // Map optimize-js block labels to LLVM basic blocks.
    let mut blocks: HashMap<u32, BasicBlock<'ctx>> = HashMap::new();
    // Only materialize reachable blocks; `CfgGraph` can contain disconnected nodes, and LLVM
    // requires every basic block in a function to end with a terminator.
    let reverse_postorder = cfg.reverse_postorder();
    for &label in &reverse_postorder {
      let bb = self
        .context
        .append_basic_block(llvm_func, &format!("b{label}"));
      blocks.insert(label, bb);
    }

    // Ensure entry block exists.
    let entry_bb = blocks
      .get(&cfg.entry)
      .copied()
      .expect("entry basic block must exist");

    // Track SSA values.
    let mut vars: HashMap<u32, BasicValueEnum<'ctx>> = HashMap::new();
    let mut var_layouts: HashMap<u32, LayoutId> = HashMap::new();
    // Track "function-typed" SSA variables for direct calls.
    let mut fn_vars: HashMap<u32, usize> = HashMap::new();

    // Bind optimize-js SSA parameter variables to LLVM function params.
    if sig.param_layouts.len() != func.params.len() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "optimize-js param count did not match resolved TypeScript signature",
        Span::new(self.entry_file, TextRange::new(0, 0)),
      )]);
    }
    for (idx, var) in func.params.iter().copied().enumerate() {
      let layout = sig.param_layouts[idx];
      let value = llvm_func.get_nth_param(idx as u32).ok_or_else(|| {
        vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "missing LLVM parameter for optimize-js function param",
          Span::new(self.entry_file, TextRange::new(0, 0)),
        )]
      })?;
      vars.insert(var, value);
      var_layouts.insert(var, layout);
    }

    // `optimize-js` retains SSA form even when `run_opt_passes = false` so native backends can
    // observe allocation markers. Without DCE, SSA construction can leave behind unused phi nodes
    // whose incoming values come from the implicit "undefined" initial state of a temp variable.
    //
    // Those pre-SSA temps do not have explicit `VarAssign` definitions in the CFG, so we must
    // ignore phi nodes that are not used by non-phi instructions in order to avoid requiring code
    // generation for values that cannot affect program semantics.
    let mut live_vars: BTreeSet<u32> = BTreeSet::new();
    // Seed with variables used by non-phi instructions (returns, branches, arithmetic, etc).
    for &label in &reverse_postorder {
      for inst in cfg.bblocks.get(label).iter() {
        if inst.t == InstTyp::Phi {
          continue;
        }
        for arg in inst.args.iter() {
          if let Arg::Var(v) = arg {
            live_vars.insert(*v);
          }
        }
      }
    }
    // Propagate liveness backwards through phi nodes until we reach a fixpoint.
    loop {
      let mut changed = false;
      for &label in &reverse_postorder {
        for inst in cfg.bblocks.get(label).iter() {
          if inst.t != InstTyp::Phi {
            continue;
          }
          let tgt = inst.tgts[0];
          if !live_vars.contains(&tgt) {
            continue;
          }
          for arg in inst.args.iter() {
            if let Arg::Var(v) = arg {
              changed |= live_vars.insert(*v);
            }
          }
        }
      }
      if !changed {
        break;
      }
    }

    // First pass: create phi nodes (without incomings).
    let mut pending_phis: Vec<(PhiValue<'ctx>, LayoutId, Vec<(u32, Arg)>)> = Vec::new();
    for &label in &reverse_postorder {
      let bb = blocks[&label];
      self.builder.position_at_end(bb);
      let insts = cfg.bblocks.get(label);
      for inst in insts.iter() {
        if inst.t != InstTyp::Phi {
          continue;
        }
        let tgt = inst.tgts[0];
        if !live_vars.contains(&tgt) {
          continue;
        }
        let layout = inst.meta.native_layout.expect("phi must have native_layout");
        let ty = llvm_type_for_layout(self.program, layout, self.context, self.ptr_gc, self.f64, self.i1, self.i64, self.i32, self.i8)?;
        let phi = self
          .builder
          .build_phi(ty, &format!("t{tgt}.phi"))
          .expect("build phi");
        vars.insert(tgt, phi.as_basic_value());
        var_layouts.insert(tgt, layout);

        // Capture incoming pairs.
        let mut incomings = Vec::with_capacity(inst.args.len());
        for (pred, arg) in inst.labels.iter().copied().zip(inst.args.iter().cloned()) {
          incomings.push((pred, arg));
        }
        pending_phis.push((phi, layout, incomings));
      }
    }

    // Second pass: emit non-phi instructions.
    for &label in &reverse_postorder {
      let bb = blocks[&label];
      self.builder.position_at_end(bb);

      let mut terminated = false;
      let insts = cfg.bblocks.get(label);

      for inst in insts.iter() {
        if inst.t == InstTyp::Phi {
          continue;
        }

          match inst.t {
            InstTyp::VarAssign => {
              let tgt = inst.tgts[0];
              match &inst.args[0] {
                Arg::Fn(fnid) => {
                  fn_vars.insert(tgt, *fnid);
                }
                Arg::Var(src) => {
                  if let Some(fnid) = fn_vars.get(src).copied() {
                    fn_vars.insert(tgt, fnid);
                    continue;
                  }
                  let layout = match inst.meta.native_layout {
                    Some(layout) => layout,
                    None => match &inst.args[0] {
                      Arg::Var(src) => *var_layouts.get(src).ok_or_else(|| {
                        vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                          "VarAssign missing native layout and RHS layout is unknown",
                          self.inst_span(inst),
                        )]
                      })?,
                      Arg::Const(Const::Num(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().number),
                      Arg::Const(Const::Bool(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().boolean),
                      Arg::Const(Const::Str(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().string),
                      _ => {
                        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                          "VarAssign missing native layout (cannot infer type from RHS)",
                          self.inst_span(inst),
                        )]);
                      }
                    },
                  };
                  let value = self.emit_value_arg(&vars, &var_layouts, inst, &inst.args[0], layout)?;
                  vars.insert(tgt, value);
                  var_layouts.insert(tgt, layout);
                }
                _ => {
                  let layout = match inst.meta.native_layout {
                    Some(layout) => layout,
                    None => match &inst.args[0] {
                      Arg::Var(src) => *var_layouts.get(src).ok_or_else(|| {
                        vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                          "VarAssign missing native layout and RHS layout is unknown",
                          self.inst_span(inst),
                        )]
                      })?,
                      Arg::Const(Const::Num(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().number),
                      Arg::Const(Const::Bool(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().boolean),
                      Arg::Const(Const::Str(_)) => self
                        .program
                        .layout_of_interned(self.program.interned_type_store().primitive_ids().string),
                      _ => {
                        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                          "VarAssign missing native layout (cannot infer type from RHS)",
                          self.inst_span(inst),
                        )]);
                      }
                    },
                  };
                  let value = self.emit_value_arg(&vars, &var_layouts, inst, &inst.args[0], layout)?;
                  vars.insert(tgt, value);
                  var_layouts.insert(tgt, layout);
                }
              }
            }

          InstTyp::Bin => {
            let tgt = inst.tgts[0];
            let layout = inst.meta.native_layout.expect("Bin must have native_layout");
            let out = self.emit_binop(&vars, &var_layouts, inst, layout, shapes)?;
            vars.insert(tgt, out);
            var_layouts.insert(tgt, layout);
          }

          InstTyp::PropAssign => {
            self.emit_prop_assign(&vars, &var_layouts, inst, shapes)?;
          }

          InstTyp::ForeignLoad => {
            let tgt = *inst.tgts.get(0).ok_or_else(|| {
              vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                "malformed foreign load (missing target)",
                self.inst_span(inst),
              )]
            })?;
            if let Some(&fnid) = foreign_fn_map.get(&inst.foreign) {
              fn_vars.insert(tgt, fnid);
            } else {
              return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                "optimize-js backend only supports foreign loads of hoisted top-level function declarations",
                self.inst_span(inst),
              )]);
            }
          }

          InstTyp::Call => {
            let out = self.emit_call(&vars, &var_layouts, &fn_vars, inst, llvm_fns, fn_sigs, shapes)?;
            if !inst.tgts.is_empty() {
              let tgt = inst.tgts[0];
              let layout = inst.meta.native_layout.expect("Call with tgt must have native_layout");
              vars.insert(tgt, out.expect("call should return value"));
              var_layouts.insert(tgt, layout);
            }
          }

          InstTyp::CondGoto => {
            let (cond, t, f) = inst.as_cond_goto();
            let cond_v = self.emit_cond_value(&vars, cond)?;
            self
              .builder
              .build_conditional_branch(cond_v, blocks[&t], blocks[&f])
              .expect("condbr");
            terminated = true;
            break;
          }

          InstTyp::Return => {
            if inst.args.is_empty() {
              self.builder.build_return(None).expect("ret void");
            } else {
              let v = inst.args[0].clone();
              let ret = self.emit_return_value(&vars, &var_layouts, inst, v, ret_kind)?;
              match ret {
                None => {
                  self.builder.build_return(None).expect("ret void");
                }
                Some(v) => {
                  self.builder.build_return(Some(&v)).expect("ret");
                }
              }
            }
            terminated = true;
            break;
          }

          InstTyp::_Dummy
          | InstTyp::_Label
          | InstTyp::_Goto
          | InstTyp::Assume
          | InstTyp::ForeignStore
          | InstTyp::UnknownLoad
          | InstTyp::UnknownStore
          | InstTyp::Throw => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              format!("unsupported optimize-js IL instruction: {:?}", inst.t),
              self.inst_span(inst),
            )]);
          }
          _ => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              format!("unsupported optimize-js IL instruction: {:?}", inst.t),
              self.inst_span(inst),
            )]);
          }
        }
      }

      if terminated {
        continue;
      }

      // Implicit terminator based on CFG edges.
      match cfg.terminator(label) {
        optimize_js::cfg::cfg::Terminator::Stop => {
          // Fallthrough with no terminator: treat as `return undefined`.
          match ret_kind {
            TsAbiKind::Void => {
              self.builder.build_return(None).expect("ret void");
            }
            TsAbiKind::Number => {
              self
                .builder
                .build_return(Some(&self.f64.const_float(0.0)))
                .expect("ret 0");
            }
            TsAbiKind::Boolean => {
              self
                .builder
                .build_return(Some(&self.i1.const_int(0, false)))
                .expect("ret false");
            }
            TsAbiKind::String => {
              // Use `InternedId::INVALID` (`u32::MAX`) for fallthrough in a string-returning
              // function. This should not occur for valid TypeScript programs.
              self
                .builder
                .build_return(Some(&self.i32.const_int(u32::MAX as u64, false)))
                .expect("ret invalid interned id");
            }
            TsAbiKind::GcPtr => {
              self
                .builder
                .build_return(Some(&self.ptr_gc.const_null()))
                .expect("ret null");
            }
          };
        }
        optimize_js::cfg::cfg::Terminator::Goto(target) => {
          self
            .builder
            .build_unconditional_branch(blocks[&target])
            .expect("br");
        }
        optimize_js::cfg::cfg::Terminator::CondGoto { cond, t, f } => {
          let cond_v = self.emit_cond_value(&vars, &cond)?;
          self
            .builder
            .build_conditional_branch(cond_v, blocks[&t], blocks[&f])
            .expect("condbr");
        }
        optimize_js::cfg::cfg::Terminator::Invoke { .. } => {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "unsupported invoke terminator in optimize-js backend",
            Span::new(self.entry_file, TextRange::new(0, 0)),
          )]);
        }
        optimize_js::cfg::cfg::Terminator::Multi { .. } => {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "unsupported multi-way branch in optimize-js backend",
            Span::new(self.entry_file, TextRange::new(0, 0)),
          )]);
        }
      }
    }

    // Final pass: populate phi incoming edges.
    for (phi, phi_layout, incomings) in pending_phis {
      let mut values: Vec<BasicValueEnum<'ctx>> = Vec::with_capacity(incomings.len());
      let mut blocks_in: Vec<BasicBlock<'ctx>> = Vec::with_capacity(incomings.len());

      for (pred, arg) in incomings {
        let pred_bb = blocks[&pred];
        let v = match arg {
          Arg::Var(id) => {
            *vars.get(&id).ok_or_else(|| {
              vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                format!("missing SSA value for phi incoming %{id} from predecessor b{pred}"),
                Span::new(self.entry_file, TextRange::new(0, 0)),
              )]
            })?
          }
          Arg::Const(Const::Num(n)) => self.f64.const_float(n.0).into(),
          Arg::Const(Const::Bool(b)) => self.i1.const_int(b as u64, false).into(),
          Arg::Const(Const::Null) | Arg::Const(Const::Undefined) => {
            let store = self.program.interned_type_store();
            match store.layout(phi_layout) {
              Layout::Ptr { .. } => self.ptr_gc.const_null().into(),
              _ => {
                return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                  "unsupported: null/undefined used as phi input for a non-pointer value",
                  Span::new(self.entry_file, TextRange::new(0, 0)),
                )]);
              }
            }
          }
          // Reject string literals in phi nodes for now: they would require a side-effecting
          // allocation in the predecessor block.
          Arg::Const(Const::Str(_)) => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              "unsupported: string literal used as phi input (requires materialization)",
              Span::new(self.entry_file, TextRange::new(0, 0)),
            )]);
          }
          other => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              format!("unsupported phi input: {other:?}"),
              Span::new(self.entry_file, TextRange::new(0, 0)),
            )]);
          }
        };
        values.push(v);
        blocks_in.push(pred_bb);
      }

      let incoming: Vec<(&dyn inkwell::values::BasicValue<'ctx>, BasicBlock<'ctx>)> = values
        .iter()
        .zip(blocks_in.iter())
        .map(|(v, bb)| (v as &dyn inkwell::values::BasicValue<'ctx>, *bb))
        .collect();
      phi.add_incoming(&incoming);
    }

    // Avoid an unused warning when the function has no blocks (shouldn't happen).
    let _ = entry_bb;

    Ok(())
  }

  fn emit_call(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    fn_vars: &HashMap<u32, usize>,
    inst: &Inst,
    llvm_fns: &[FunctionValue<'ctx>],
    fn_sigs: &[FnSig],
    shapes: &ShapeTable,
  ) -> Result<Option<BasicValueEnum<'ctx>>, Vec<Diagnostic>> {
    let (_tgt, callee, this_arg, args, spreads) = inst.as_call();
    if !spreads.is_empty() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "call arguments with spread are not supported in optimize-js backend",
        self.inst_span(inst),
      )]);
    }

    if let Arg::Builtin(name) = callee {
      return match name.as_str() {
        "__optimize_js_object" => {
          let obj = self.emit_alloc_object(vars, var_layouts, inst, args, shapes)?;
          Ok(Some(obj.into()))
        }
        "__optimize_js_array" => {
          let arr = self.emit_alloc_array(vars, var_layouts, inst, args)?;
          Ok(Some(arr.into()))
        }
        other => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          format!("unsupported builtin call in optimize-js backend: {other}"),
          self.inst_span(inst),
        )]),
      };
    }

    // Direct call lowering.
    if !matches!(this_arg, Arg::Const(Const::Undefined)) {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "optimize-js backend only supports direct calls (callee must have `this=undefined`)",
        self.inst_span(inst),
      )]);
    }

    let fnid = match callee {
      Arg::Fn(id) => *id,
      Arg::Var(v) => fn_vars.get(v).copied().ok_or_else(|| {
        vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "optimize-js backend only supports direct calls to known functions",
          self.inst_span(inst),
        )]
      })?,
      _ => {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "optimize-js backend only supports direct calls to known functions",
          self.inst_span(inst),
        )]);
      }
    };

    let callee_fn = *llvm_fns.get(fnid).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        format!("call references unknown function id Fn{fnid}"),
        self.inst_span(inst),
      )]
    })?;
    let sig = fn_sigs.get(fnid).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        format!("missing signature for function id Fn{fnid}"),
        self.inst_span(inst),
      )]
    })?;

    if args.len() != sig.param_layouts.len() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "optimize-js call argument count did not match TypeScript signature",
        self.inst_span(inst),
      )]);
    }

    let mut llvm_args = Vec::with_capacity(args.len());
    for (arg, &expected_layout) in args.iter().zip(sig.param_layouts.iter()) {
      llvm_args.push(self.emit_value_arg(vars, var_layouts, inst, arg, expected_layout)?.into());
    }

    let call = self
      .builder
      .build_call(callee_fn, &llvm_args, "call")
      .map_err(|e| vec![diagnostics::ice(self.inst_span(inst), format!("failed to build call: {e}"))])?;
    crate::stack_walking::mark_call_notail(call);
    Ok(call.try_as_basic_value().left())
  }

  fn emit_alloc_object(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    args: &[Arg],
    shapes: &ShapeTable,
  ) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    if !inst.spreads.is_empty() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "object literals with spread are not supported",
        self.inst_span(inst),
      )]);
    }
    if args.len() % 3 != 0 {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "invalid __optimize_js_object argument encoding",
        self.inst_span(inst),
      )]);
    }

    let layout = inst.meta.native_layout.ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "missing native layout for object literal (typed mode is required)",
        self.inst_span(inst),
      )]
    })?;

    let store = self.program.interned_type_store();
    let Layout::Ptr { to } = store.layout(layout) else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "object literal did not have a pointer layout",
        self.inst_span(inst),
      )]);
    };
    let PtrKind::GcObject { layout: payload } = to else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "object literal did not have a GC object layout",
        self.inst_span(inst),
      )]);
    };

    let shape = shapes.get_by_payload(payload).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "missing shape table entry for object payload layout",
        self.inst_span(inst),
      )]
    })?;

    // rt_alloc(object_size, shape_id)
    let abi = RuntimeAbi::new(self.context, &self.module);
    let call = abi
      .emit_runtime_call(
        &self.builder,
        RuntimeFn::Alloc,
        &[
          self.i64.const_int(shape.object_size as u64, false).into(),
          self.i32.const_int(shape.shape_id_raw as u64, false).into(),
        ],
        "rt.alloc",
      )
      .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
    let obj = call
      .try_as_basic_value()
      .left()
      .expect("rt_alloc returns value")
      .into_pointer_value();

    // Initialize all GC pointer slots to null before any other safepoint.
    for off in &shape.ptr_offsets {
      let slot = self.gc_ptr_slot_ptr(obj, *off);
      self.builder.build_store(slot, self.ptr_gc.const_null()).expect("store null");
    }

    // Store explicit fields.
    let payload_layout = store.layout(payload);
    let Layout::Struct { fields, .. } = payload_layout else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "object payload layout is not a struct",
        self.inst_span(inst),
      )]);
    };

    for chunk in args.chunks_exact(3) {
      let marker = &chunk[0];
      let key = &chunk[1];
      let value = &chunk[2];

      let Arg::Builtin(marker) = marker else {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "invalid object literal marker",
          self.inst_span(inst),
        )]);
      };
      if marker != "__optimize_js_object_prop" {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "only constant-key object literals are supported (no computed keys/spread)",
          self.inst_span(inst),
        )]);
      }

      let key_str = match key {
        Arg::Const(Const::Str(s)) => s,
        _ => {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "unsupported object literal key",
            self.inst_span(inst),
          )])
        }
      };

      let name_id = store.intern_name_ref(key_str);
      let field_key = FieldKey::Prop(types_ts_interned::PropKey::String(name_id));
      let field = fields.iter().find(|f| f.key == field_key).ok_or_else(|| {
        vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
          format!("unknown object field `{key_str}` for native layout"),
          self.inst_span(inst),
        )]
      })?;

      let abs_off = shape.payload_base_offset + field.offset;
      let value_layout = field.layout;
      self.emit_store_to_object_field(vars, var_layouts, inst, obj, abs_off, value_layout, value)?;
    }

    Ok(obj)
  }

  fn emit_store_to_object_field(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    obj: PointerValue<'ctx>,
    abs_off: u32,
    field_layout: LayoutId,
    value: &Arg,
  ) -> Result<(), Vec<Diagnostic>> {
    let store = self.program.interned_type_store();
    match store.layout(field_layout) {
      Layout::Ptr { to } if to.is_gc_tracable() => {
        // Evaluate value (may allocate) before computing derived slot pointer.
        let v = self.emit_value_arg(vars, var_layouts, inst, value, field_layout)?;
        let v = v.into_pointer_value();
        let slot = self.gc_ptr_slot_ptr(obj, abs_off);
        self.builder.build_store(slot, v).expect("store gc ptr");
        RuntimeAbi::new(self.context, &self.module)
          .emit_runtime_call(
            &self.builder,
            RuntimeFn::WriteBarrier,
            &[obj.into(), slot.into()],
            "rt.wb",
          )
          .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
      }
      Layout::Scalar { abi } => {
        let v = self.emit_value_arg(vars, var_layouts, inst, value, field_layout)?;
        let addr = self.byte_ptr(obj, abs_off);
        match abi {
          AbiScalar::F64 => {
            self.builder.build_store(addr, v.into_float_value()).expect("store f64");
          }
          AbiScalar::Bool => {
            self.builder.build_store(addr, v.into_int_value()).expect("store bool");
          }
          AbiScalar::I32 | AbiScalar::U32 => {
            self.builder.build_store(addr, v.into_int_value()).expect("store i32");
          }
          AbiScalar::I64 | AbiScalar::U64 => {
            self.builder.build_store(addr, v.into_int_value()).expect("store i64");
          }
          _ => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              "unsupported scalar field type in optimize-js backend",
              self.inst_span(inst),
            )]);
          }
        }
      }
      _ => {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "unsupported object field layout in optimize-js backend",
          self.inst_span(inst),
        )]);
      }
    }
    Ok(())
  }

  fn emit_alloc_array(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    args: &[Arg],
  ) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    if !inst.spreads.is_empty() {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "array literals with spread are not supported",
        self.inst_span(inst),
      )]);
    }
    for arg in args {
      if matches!(arg, Arg::Builtin(name) if name == "__optimize_js_array_hole") {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
          "array literals with holes are not supported",
          self.inst_span(inst),
        )]);
      }
    }

    let layout = inst.meta.native_layout.ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "missing native layout for array literal (typed mode is required)",
        self.inst_span(inst),
      )]
    })?;

    let store = self.program.interned_type_store();
    let Layout::Ptr { to } = store.layout(layout) else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "array literal did not have a pointer layout",
        self.inst_span(inst),
      )]);
    };
    let PtrKind::GcArray { elem } = to else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "array literal did not have a GC array layout",
        self.inst_span(inst),
      )]);
    };

    let len = args.len();
    let elem_repr = store.array_elem_repr(elem);

    let (elem_size_arg, ptr_elems) = match elem_repr {
      ArrayElemRepr::PlainOldData { elem_size, .. } => (elem_size as u64, false),
      ArrayElemRepr::GcPointer => (
        (runtime_native_abi::RT_ARRAY_ELEM_PTR_FLAG | runtime_native_abi::RT_PTR_SIZE_BYTES) as u64,
        true,
      ),
      ArrayElemRepr::NeedsBoxing => {
        return Err(vec![codes::OPTIMIZE_JS_ARRAY_ELEM_NEEDS_BOXING.error(
          "array element layout requires boxing and is not supported yet",
          self.inst_span(inst),
        )]);
      }
    };

    let abi = RuntimeAbi::new(self.context, &self.module);
    let call = abi
      .emit_runtime_call(
        &self.builder,
        RuntimeFn::AllocArray,
        &[
          self.i64.const_int(len as u64, false).into(),
          self.i64.const_int(elem_size_arg, false).into(),
        ],
        "rt.alloc_array",
      )
      .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
    let arr = call
      .try_as_basic_value()
      .left()
      .expect("rt_alloc_array returns value")
      .into_pointer_value();

    if ptr_elems {
      // Initialize all element slots to null before any safepoint.
      let base = (runtime_native_abi::RT_ARRAY_DATA_OFFSET / runtime_native_abi::RT_PTR_SIZE_BYTES) as u64;
      for i in 0..len {
        let idx = self
          .i64
          .const_int(base + (i as u64), false);
        let slot = unsafe { self.builder.build_gep(self.ptr_gc, arr, &[idx], "arr.slot") }.expect("gep");
        self.builder.build_store(slot, self.ptr_gc.const_null()).expect("store null");
      }
    }

    // Store element values.
    for (i, elem_arg) in args.iter().enumerate() {
      if ptr_elems {
        let value = self.emit_value_arg(vars, var_layouts, inst, elem_arg, elem)?;
        let value = value.into_pointer_value();
        let base = (runtime_native_abi::RT_ARRAY_DATA_OFFSET / runtime_native_abi::RT_PTR_SIZE_BYTES) as u64;
        let idx = self.i64.const_int(base + (i as u64), false);
        let slot = unsafe { self.builder.build_gep(self.ptr_gc, arr, &[idx], "arr.slot") }.expect("gep");
        self.builder.build_store(slot, value).expect("store");
        RuntimeAbi::new(self.context, &self.module)
          .emit_runtime_call(
            &self.builder,
            RuntimeFn::WriteBarrier,
            &[arr.into(), slot.into()],
            "rt.wb",
          )
          .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
      } else {
        let value = self.emit_value_arg(vars, var_layouts, inst, elem_arg, elem)?;
        let elem_size = match store.array_elem_repr(elem) {
          ArrayElemRepr::PlainOldData { elem_size, .. } => elem_size,
          _ => 1,
        };
        let off = (runtime_native_abi::RT_ARRAY_DATA_OFFSET as u32)
          + (i as u32).saturating_mul(elem_size);
        let addr = self.byte_ptr(arr, off);
        self.builder.build_store(addr, value).expect("store elem");
      }
    }

    Ok(arr)
  }

  fn emit_prop_assign(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    shapes: &ShapeTable,
  ) -> Result<(), Vec<Diagnostic>> {
    let (obj, key, value) = inst.as_prop_assign();
    let obj_var = obj.to_var();
    let obj_layout = *var_layouts.get(&obj_var).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        "missing native layout for property receiver",
        self.inst_span(inst),
      )]
    })?;

    let store = self.program.interned_type_store();
    let Layout::Ptr { to } = store.layout(obj_layout) else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        "property receiver is not a pointer type",
        self.inst_span(inst),
      )]);
    };
    let PtrKind::GcObject { layout: payload } = to else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        "property receiver is not a GC object with a known payload layout",
        self.inst_span(inst),
      )]);
    };

    let key_str = match &key {
      Arg::Const(Const::Str(s)) => s.clone(),
      Arg::Const(Const::Num(n)) => n.0.to_string(),
      _ => {
        return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
          "dynamic property keys are not supported",
          self.inst_span(inst),
        )])
      }
    };

    let shape = shapes.get_by_payload(payload).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        "missing shape info for receiver payload",
        self.inst_span(inst),
      )]
    })?;
    let payload_layout = store.layout(payload);
    let Layout::Struct { fields, .. } = payload_layout else {
      return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        "receiver payload layout is not a struct",
        self.inst_span(inst),
      )]);
    };
    let name_id = store.intern_name_ref(&key_str);
    let field_key = FieldKey::Prop(types_ts_interned::PropKey::String(name_id));
    let field = fields.iter().find(|f| f.key == field_key).ok_or_else(|| {
      vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
        format!("unknown object field `{key_str}`"),
        self.inst_span(inst),
      )]
    })?;

    let obj_ptr = vars
      .get(&obj_var)
      .expect("receiver value")
      .into_pointer_value();
    let abs_off = shape.payload_base_offset + field.offset;

    self.emit_store_to_object_field(vars, var_layouts, inst, obj_ptr, abs_off, field.layout, &value)?;

    Ok(())
  }

  fn emit_binop(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    out_layout: LayoutId,
    shapes: &ShapeTable,
  ) -> Result<BasicValueEnum<'ctx>, Vec<Diagnostic>> {
    let left = inst.args[0].clone();
    let right = inst.args[1].clone();

    match inst.bin_op {
      BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
        let a = self.emit_value_arg(vars, var_layouts, inst, &left, out_layout)?.into_float_value();
        let b = self.emit_value_arg(vars, var_layouts, inst, &right, out_layout)?.into_float_value();
        let v = match inst.bin_op {
          BinOp::Add => self.builder.build_float_add(a, b, "add").expect("fadd").into(),
          BinOp::Sub => self.builder.build_float_sub(a, b, "sub").expect("fsub").into(),
          BinOp::Mul => self.builder.build_float_mul(a, b, "mul").expect("fmul").into(),
          BinOp::Div => self.builder.build_float_div(a, b, "div").expect("fdiv").into(),
          BinOp::Mod => self.builder.build_float_rem(a, b, "rem").expect("frem").into(),
          _ => unreachable!(),
        };
        Ok(v)
      }

      BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq => {
        let a = self.emit_value_arg(vars, var_layouts, inst, &left, out_layout)?.into_float_value();
        let b = self.emit_value_arg(vars, var_layouts, inst, &right, out_layout)?.into_float_value();
        let pred = match inst.bin_op {
          BinOp::Lt => FloatPredicate::OLT,
          BinOp::Leq => FloatPredicate::OLE,
          BinOp::Gt => FloatPredicate::OGT,
          BinOp::Geq => FloatPredicate::OGE,
          _ => unreachable!(),
        };
        Ok(self.builder.build_float_compare(pred, a, b, "cmp").expect("fcmp").into())
      }

      BinOp::StrictEq | BinOp::NotStrictEq => {
        // Only support scalar comparisons used by simple loops.
        //
        // Note: `out_layout` is the boolean result layout; operand layouts must be derived from the
        // operand values so `Const::Str` can be lowered according to `types-ts-interned`'s native
        // layout (strings are `u32` interned IDs).
        let store = self.program.interned_type_store();
        let primitives = store.primitive_ids();
        let arg_layout = |arg: &Arg| -> Result<LayoutId, Vec<Diagnostic>> {
          match arg {
            Arg::Var(v) => var_layouts.get(v).copied().ok_or_else(|| {
              vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                "missing native layout for StrictEq operand",
                self.inst_span(inst),
              )]
            }),
            Arg::Const(Const::Num(_)) => Ok(self.program.layout_of_interned(primitives.number)),
            Arg::Const(Const::Bool(_)) => Ok(self.program.layout_of_interned(primitives.boolean)),
            Arg::Const(Const::Str(_)) => Ok(self.program.layout_of_interned(primitives.string)),
            _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              "unsupported StrictEq operand kind in optimize-js backend",
              self.inst_span(inst),
            )]),
          }
        };
        let a_layout = arg_layout(&left)?;
        let b_layout = arg_layout(&right)?;
        let a = self.emit_value_arg(vars, var_layouts, inst, &left, a_layout)?;
        let b = self.emit_value_arg(vars, var_layouts, inst, &right, b_layout)?;
        let eq = match (a, b) {
          (BasicValueEnum::FloatValue(a), BasicValueEnum::FloatValue(b)) => self
            .builder
            .build_float_compare(FloatPredicate::OEQ, a, b, "eq")
            .expect("fcmp"),
          (BasicValueEnum::IntValue(a), BasicValueEnum::IntValue(b)) => self
            .builder
            .build_int_compare(IntPredicate::EQ, a, b, "eq")
            .expect("icmp"),
          (BasicValueEnum::PointerValue(_), BasicValueEnum::PointerValue(_)) => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              "unsupported StrictEq operand types in optimize-js backend (pointer comparisons are not supported yet)",
              self.inst_span(inst),
            )]);
          }
          _ => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
              "unsupported StrictEq operand types in optimize-js backend",
              self.inst_span(inst),
            )]);
          }
        };
        if inst.bin_op == BinOp::NotStrictEq {
          Ok(self.builder.build_not(eq, "neq").expect("not").into())
        } else {
          Ok(eq.into())
        }
      }

      BinOp::GetProp => {
        let obj_var = left.to_var();
        let obj_layout = *var_layouts.get(&obj_var).ok_or_else(|| {
          vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            "missing native layout for property receiver",
            self.inst_span(inst),
          )]
        })?;

        let store = self.program.interned_type_store();
        let Layout::Ptr { to } = store.layout(obj_layout) else {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            "property receiver is not a pointer type",
            self.inst_span(inst),
          )]);
        };

        // Special-case string.length
        if matches!(to, PtrKind::GcString) {
          let key = match right {
            Arg::Const(Const::Str(s)) => s,
            _ => {
              return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
                "dynamic property keys are not supported",
                self.inst_span(inst),
              )])
            }
          };
          if key != "length" {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
              "only `string.length` is supported on GC strings",
              self.inst_span(inst),
            )]);
          }
          let s_ptr = vars.get(&obj_var).expect("string").into_pointer_value();
          let call = RuntimeAbi::new(self.context, &self.module)
            .emit_runtime_call(&self.builder, RuntimeFn::StringLen, &[s_ptr.into()], "rt.string.len")
            .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
          let len_i64 = call
            .try_as_basic_value()
            .left()
            .expect("string_len returns")
            .into_int_value();
          let len_f64 = self
            .builder
            .build_unsigned_int_to_float(len_i64, self.f64, "len.f64")
            .expect("uitofp");
          return Ok(len_f64.into());
        }

        let PtrKind::GcObject { layout: payload } = to else {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            "property receiver is not a GC object with a known payload layout",
            self.inst_span(inst),
          )]);
        };

        let key = match right {
          Arg::Const(Const::Str(s)) => s,
          Arg::Const(Const::Num(n)) => n.0.to_string(),
          _ => {
            return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
              "dynamic property keys are not supported",
              self.inst_span(inst),
            )])
          }
        };

        let shape = shapes.get_by_payload(payload).ok_or_else(|| {
          vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            "missing shape info for receiver payload",
            self.inst_span(inst),
          )]
        })?;
        let payload_layout = store.layout(payload);
        let Layout::Struct { fields, .. } = payload_layout else {
          return Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            "receiver payload layout is not a struct",
            self.inst_span(inst),
          )]);
        };
        let name_id = store.intern_name_ref(&key);
        let field_key = FieldKey::Prop(types_ts_interned::PropKey::String(name_id));
        let field = fields.iter().find(|f| f.key == field_key).ok_or_else(|| {
          vec![codes::OPTIMIZE_JS_UNSUPPORTED_PROPERTY_ACCESS.error(
            format!("unknown object field `{key}`"),
            self.inst_span(inst),
          )]
        })?;

        let obj_ptr = vars
          .get(&obj_var)
          .expect("receiver value")
          .into_pointer_value();
        let abs_off = shape.payload_base_offset + field.offset;

        match store.layout(field.layout) {
          Layout::Ptr { to } if to.is_gc_tracable() => {
            let slot = self.gc_ptr_slot_ptr(obj_ptr, abs_off);
            let loaded = self
              .builder
              .build_load(self.ptr_gc, slot, "ld.ptr")
              .expect("load ptr")
              .into_pointer_value();
            Ok(loaded.into())
          }
          Layout::Scalar { abi } => {
            let addr = self.byte_ptr(obj_ptr, abs_off);
            match abi {
              AbiScalar::F64 => Ok(
                self
                  .builder
                  .build_load(self.f64, addr, "ld.f64")
                  .expect("load f64")
                  .into(),
              ),
              AbiScalar::Bool => Ok(
                self
                  .builder
                  .build_load(self.i1, addr, "ld.bool")
                  .expect("load bool")
                  .into(),
              ),
              AbiScalar::I32 | AbiScalar::U32 => Ok(
                self
                  .builder
                  .build_load(self.i32, addr, "ld.i32")
                  .expect("load i32")
                  .into(),
              ),
              AbiScalar::I64 | AbiScalar::U64 => Ok(
                self
                  .builder
                  .build_load(self.i64, addr, "ld.i64")
                  .expect("load i64")
                  .into(),
              ),
              _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
                "unsupported scalar field type in optimize-js backend",
                self.inst_span(inst),
              )]),
            }
          }
          _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "unsupported field layout in optimize-js backend",
            self.inst_span(inst),
          )]),
        }
      }

      _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        format!("unsupported binary op in optimize-js backend: {:?}", inst.bin_op),
        self.inst_span(inst),
      )]),
    }
  }

  fn emit_value_arg(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    _var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    arg: &Arg,
    expected_layout: LayoutId,
  ) -> Result<BasicValueEnum<'ctx>, Vec<Diagnostic>> {
    match arg {
      Arg::Var(v) => Ok(*vars.get(v).expect("var value")),
      Arg::Const(c) => self.emit_const_value(inst, c, expected_layout),
      _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "unsupported arg kind in optimize-js backend",
        self.inst_span(inst),
      )]),
    }
  }

  fn emit_const_value(
    &mut self,
    inst: &Inst,
    c: &Const,
    expected_layout: LayoutId,
  ) -> Result<BasicValueEnum<'ctx>, Vec<Diagnostic>> {
    match c {
      Const::Num(n) => Ok(self.f64.const_float(n.0).into()),
      Const::Bool(b) => Ok(self.i1.const_int(*b as u64, false).into()),
      Const::Str(s) => {
        let store = self.program.interned_type_store();
        match store.layout(expected_layout) {
          Layout::Scalar {
            abi: AbiScalar::U32 | AbiScalar::I32,
          } => Ok(self.emit_string_literal(inst, s)?.into()),
          Layout::Ptr { to: PtrKind::GcString } => Ok(self.emit_gc_string_literal(inst, s)?.into()),
          _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "string literal used in an unsupported native layout position",
            self.inst_span(inst),
          )]),
        }
      }
      Const::Null | Const::Undefined => {
        // Nullish literals should only appear in pointer-typed positions for the current backend.
        let store = self.program.interned_type_store();
        match store.layout(expected_layout) {
          Layout::Ptr { .. } => Ok(self.ptr_gc.const_null().into()),
          _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
            "unsupported nullish literal in non-pointer position",
            self.inst_span(inst),
          )]),
        }
      }
      Const::BigInt(_) => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "bigint literals are not supported in optimize-js backend",
        self.inst_span(inst),
      )]),
    }
  }

  fn string_bytes_global(&mut self, s: &str) -> GlobalValue<'ctx> {
    if let Some(gv) = self.string_bytes_globals.get(s) {
      return *gv;
    }

    let id = self.next_string_global_id;
    self.next_string_global_id += 1;
    let bytes = s.as_bytes();
    let arr_ty = self.i8.array_type(bytes.len() as u32);
    let values: Vec<_> = bytes.iter().map(|b| self.i8.const_int(*b as u64, false)).collect();
    let init = self.i8.const_array(&values);
    let gv = self.module.add_global(arr_ty, None, &format!("__nativejs_str_bytes_{id}"));
    gv.set_linkage(Linkage::Internal);
    gv.set_constant(true);
    gv.set_initializer(&init);
    self.string_bytes_globals.insert(s.to_string(), gv);
    gv
  }

  fn emit_string_literal(&mut self, inst: &Inst, s: &str) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let gv = self.string_bytes_global(s);
    let bytes_ptr = gv.as_pointer_value();
    let len = self.i64.const_int(s.as_bytes().len() as u64, false);

    let abi = RuntimeAbi::new(self.context, &self.module);
    let call = abi
      .emit_runtime_call(
        &self.builder,
        RuntimeFn::StringIntern,
        &[bytes_ptr.into(), len.into()],
        "rt.string.intern",
      )
      .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
    let id = call
      .try_as_basic_value()
      .left()
      .expect("rt_string_intern returns")
      .into_int_value();

    // Strings are represented as `InternedId` (`u32`) scalars. Since GC tracing does not see these
    // scalar IDs inside object/array payloads, we must pin them so they remain valid for the
    // lifetime of the program.
    let _ = abi
      .emit_runtime_call(&self.builder, RuntimeFn::StringPinInterned, &[id.into()], "rt.string.pin")
      .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
    Ok(id)
  }

  #[allow(dead_code)]
  fn emit_gc_string_literal(&mut self, inst: &Inst, s: &str) -> Result<PointerValue<'ctx>, Vec<Diagnostic>> {
    let gv = self.string_bytes_global(s);
    let bytes_ptr = gv.as_pointer_value();
    let len = self.i64.const_int(s.as_bytes().len() as u64, false);
    let call = RuntimeAbi::new(self.context, &self.module)
      .emit_runtime_call(
        &self.builder,
        RuntimeFn::StringNewUtf8,
        &[bytes_ptr.into(), len.into()],
        "rt.string.new",
      )
      .map_err(|e| vec![runtime_call_error_to_diag(e, self.inst_span(inst))])?;
    Ok(
      call
        .try_as_basic_value()
        .left()
        .expect("string_new returns")
        .into_pointer_value(),
    )
  }

  fn emit_cond_value(&self, vars: &HashMap<u32, BasicValueEnum<'ctx>>, arg: &Arg) -> Result<inkwell::values::IntValue<'ctx>, Vec<Diagnostic>> {
    match arg {
      Arg::Var(id) => Ok(vars.get(id).expect("cond var").into_int_value()),
      Arg::Const(Const::Bool(b)) => Ok(self.i1.const_int(*b as u64, false)),
      _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "unsupported condition value in optimize-js backend",
        Span::new(self.entry_file, TextRange::new(0, 0)),
      )]),
    }
  }

  fn emit_return_value(
    &mut self,
    vars: &HashMap<u32, BasicValueEnum<'ctx>>,
    var_layouts: &HashMap<u32, LayoutId>,
    inst: &Inst,
    arg: Arg,
    ret_kind: TsAbiKind,
  ) -> Result<Option<BasicValueEnum<'ctx>>, Vec<Diagnostic>> {
    match ret_kind {
      TsAbiKind::Void => Ok(None),
      TsAbiKind::Number => Ok(Some(self.emit_value_arg(vars, var_layouts, inst, &arg, inst.meta.native_layout.unwrap_or_else(|| self.program.layout_of_interned(self.program.interned_type_store().primitive_ids().number)))?)),
      TsAbiKind::Boolean => Ok(Some(self.emit_value_arg(vars, var_layouts, inst, &arg, inst.meta.native_layout.unwrap_or_else(|| self.program.layout_of_interned(self.program.interned_type_store().primitive_ids().boolean)))?)),
      TsAbiKind::String => Ok(Some(self.emit_value_arg(
        vars,
        var_layouts,
        inst,
        &arg,
        inst
          .meta
          .native_layout
          .unwrap_or_else(|| self.program.layout_of_interned(self.program.interned_type_store().primitive_ids().string)),
      )?)),
      TsAbiKind::GcPtr => Ok(Some(self.emit_value_arg(vars, var_layouts, inst, &arg, inst.meta.native_layout.unwrap_or_else(|| self.program.layout_of_interned(self.program.interned_type_store().primitive_ids().unknown)))?)),
    }
  }

  fn gc_ptr_slot_ptr(&self, obj: PointerValue<'ctx>, byte_off: u32) -> PointerValue<'ctx> {
    debug_assert!(byte_off % (runtime_native_abi::RT_PTR_SIZE_BYTES as u32) == 0);
    let idx = self.i64.const_int((byte_off / (runtime_native_abi::RT_PTR_SIZE_BYTES as u32)) as u64, false);
    // SAFETY: `obj` is a runtime-native allocation base pointer and `idx` is derived from a
    // byte offset within that allocation. The resulting pointer is used only for field access
    // inside the same object.
    unsafe { self.builder.build_gep(self.ptr_gc, obj, &[idx], "slot") }.expect("gep slot")
  }

  fn byte_ptr(&self, base: PointerValue<'ctx>, byte_off: u32) -> PointerValue<'ctx> {
    let idx = self.i64.const_int(byte_off as u64, false);
    // SAFETY: `base` is a runtime-native allocation base pointer and `byte_off` is an in-bounds
    // byte offset within that allocation.
    unsafe { self.builder.build_gep(self.i8, base, &[idx], "byte") }.expect("gep byte")
  }

  fn inst_span(&self, inst: &Inst) -> Span {
    let file = self.entry_file;
    let range = inst.meta.span.unwrap_or_else(|| TextRange::new(0, 0));
    Span::new(file, range)
  }
}

fn align_up(offset: u32, align: u32) -> u32 {
  debug_assert!(align != 0);
  let rem = offset % align;
  if rem == 0 {
    offset
  } else {
    offset + (align - rem)
  }
}

fn collect_foreign_fn_map(cfg: &Cfg) -> HashMap<SymbolId, usize> {
  let mut map = HashMap::new();
  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts.iter() {
      if inst.t != InstTyp::ForeignStore {
        continue;
      }
      let Some(arg) = inst.args.get(0) else {
        continue;
      };
      let Arg::Fn(fnid) = arg else {
        continue;
      };
      map.insert(inst.foreign, *fnid);
    }
  }
  map
}

fn collect_object_payloads_from_cfg(
  store: &types_ts_interned::TypeStore,
  file: FileId,
  cfg: &optimize_js::cfg::cfg::Cfg,
  out: &mut BTreeMap<LayoutId, TextRange>,
) -> Result<(), Vec<Diagnostic>> {
  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
      let Arg::Builtin(name) = callee else {
        continue;
      };
      if name != "__optimize_js_object" {
        continue;
      }
      let Some(layout) = inst.meta.native_layout else {
        continue;
      };
      let Layout::Ptr { to } = store.layout(layout) else {
        continue;
      };
      let PtrKind::GcObject { layout: payload } = to else {
        continue;
      };
      let span = inst.meta.span.unwrap_or_else(|| TextRange::new(0, 0));
      out.entry(payload).or_insert(span);
    }
  }
  let _ = file;
  Ok(())
}

fn llvm_type_for_layout<'ctx>(
  program: &Program,
  layout: LayoutId,
  _ctx: &'ctx Context,
  ptr_gc: inkwell::types::PointerType<'ctx>,
  f64: FloatType<'ctx>,
  i1: IntType<'ctx>,
  i64: IntType<'ctx>,
  i32: IntType<'ctx>,
  i8: IntType<'ctx>,
) -> Result<BasicTypeEnum<'ctx>, Vec<Diagnostic>> {
  let store = program.interned_type_store();
  match store.layout(layout) {
    Layout::Scalar { abi } => match abi {
      AbiScalar::F64 => Ok(f64.into()),
      AbiScalar::Bool => Ok(i1.into()),
      AbiScalar::I64 | AbiScalar::U64 => Ok(i64.into()),
      AbiScalar::I32 | AbiScalar::U32 => Ok(i32.into()),
      AbiScalar::U8 => Ok(i8.into()),
      _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
        "unsupported scalar layout in optimize-js backend",
        Span::new(FileId(0), TextRange::new(0, 0)),
      )]),
    },
    Layout::Ptr { to } if to.is_gc_tracable() => Ok(ptr_gc.into()),
    _ => Err(vec![codes::OPTIMIZE_JS_UNSUPPORTED_IL.error(
      "unsupported layout in optimize-js backend",
      Span::new(FileId(0), TextRange::new(0, 0)),
    )]),
  }
}

fn runtime_call_error_to_diag(err: RuntimeCallError, span: Span) -> Diagnostic {
  codes::OPTIMIZE_JS_RUNTIME_CALL_ERROR.error(err.to_string(), span)
}
