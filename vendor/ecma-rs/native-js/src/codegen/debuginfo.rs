use diagnostics::FileId;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
  AsDIScope, DICompileUnit, DIExpression, DIFile, DILocation, DILocalVariable, DISubprogram, DIType,
  DWARFEmissionKind, DWARFSourceLanguage, DebugInfoBuilder,
};
use inkwell::module::Module;
use inkwell::values::{AsValueRef, BasicValueEnum, GlobalValue, PointerValue};
use llvm_sys::core::{
  LLVMAddFunction, LLVMBuildCall2, LLVMFunctionType, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMMetadataAsValue, LLVMMetadataTypeInContext, LLVMSetCurrentDebugLocation2, LLVMValueAsMetadata,
  LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMMetadataRef, LLVMTypeRef, LLVMValueRef};
use std::ffi::CString;
use std::collections::HashMap;
use std::os::raw::c_uint;
use typecheck_ts::Program;

use crate::OptLevel;
use super::TsAbiKind;

/// Debug info state for the HIR-driven native-js codegen backend.
///
/// This provides enough DWARF metadata for basic source-level debugging:
/// - compile unit + file entries
/// - function/subprogram metadata
/// - instruction-level `!dbg` locations (line tables)
/// - parameter/local variable locations (`llvm.dbg.declare` / `llvm.dbg.value`)
pub(crate) struct CodegenDebug<'ctx> {
  optimized: bool,
  builder: DebugInfoBuilder<'ctx>,
  compile_unit: DICompileUnit<'ctx>,
  files: HashMap<FileId, DIFile<'ctx>>,
  types: DebugTypes<'ctx>,
}

#[derive(Clone, Copy)]
struct DebugTypes<'ctx> {
  number: DIType<'ctx>,
  boolean: DIType<'ctx>,
}

impl<'ctx> CodegenDebug<'ctx> {
  pub(crate) fn new(module: &Module<'ctx>, program: &Program, entry_file: FileId, opt_level: OptLevel) -> Self {
    let optimized = !matches!(opt_level, OptLevel::O0);
    // `inkwell` bundles compile-unit creation into `Module::create_debug_info_builder` and returns
    // both the `DebugInfoBuilder` and the newly created `DICompileUnit`.
    //
    // We use the entry file as the compile unit file; individual functions/variables will
    // reference their real source files via `DIFile` entries.
    let entry_name = program
      .file_key(entry_file)
      .map(|k| k.to_string())
      .unwrap_or_else(|| "entry.ts".to_string());
    let (builder, compile_unit) = module.create_debug_info_builder(
      true,
      DWARFSourceLanguage::C,
      &entry_name,
      ".",
      "native-js",
      optimized,
      "",
      0,
      "",
      DWARFEmissionKind::Full,
      0,
      false,
      false,
      "",
      "",
    );

    let types = DebugTypes {
      // DWARF type encodings.
      //
      // The LLVM C API exposes these as `LLVMDWARFTypeEncoding` values. In inkwell 0.5 + LLVM 18
      // they are plain integers, so we just use the DWARF numeric codes directly.
      //
      // Ref: DWARF v5, section 7.8 "Type encodings".
      // `number` is represented as an IEEE-754 double (`f64`) in the current backend.
      // DW_ATE_float = 0x04.
      number: builder
        .create_basic_type("number", 64, 0x04, 0)
        .expect("failed to create `number` debug type")
        .as_type(),
      // Use an 8-bit boolean for friendlier debugger display. Values are stored as `i1` (0/1) in
      // the current backend; reading the low byte yields a usable 0/1 representation.
      boolean: builder
        .create_basic_type("boolean", 8, 0x02, 0)
        .expect("failed to create `boolean` debug type")
        .as_type(),
    };

    Self {
      optimized,
      builder,
      compile_unit,
      files: HashMap::new(),
      types,
    }
  }

  pub(crate) fn finalize(&self) {
    self.builder.finalize();
  }

  pub(crate) fn optimized(&self) -> bool {
    self.optimized
  }

  pub(crate) fn file(&mut self, program: &Program, file: FileId) -> DIFile<'ctx> {
    if let Some(existing) = self.files.get(&file).copied() {
      return existing;
    }

    let name = program
      .file_key(file)
      .map(|k| k.to_string())
      .unwrap_or_else(|| format!("file{}.ts", file.0));
    let di_file = self.builder.create_file(&name, ".");
    self.files.insert(file, di_file);
    di_file
  }

  pub(crate) fn basic_type(&self, kind: TsAbiKind) -> DIType<'ctx> {
    match kind {
      TsAbiKind::Number => self.types.number,
      TsAbiKind::Boolean => self.types.boolean,
      // There is no value type for `void`; treat it as `number` to keep the debug metadata builder
      // happy (return type does not materially affect variable inspection in our current subset).
      TsAbiKind::Void => self.types.number,
    }
  }

  pub(crate) fn create_subprogram(
    &mut self,
    program: &Program,
    file: FileId,
    name: &str,
    line: u32,
    return_type: TsAbiKind,
    param_types: &[TsAbiKind],
    function: inkwell::values::FunctionValue<'ctx>,
  ) -> DISubprogram<'ctx> {
    let di_file = self.file(program, file);

    let return_ty = match return_type {
      TsAbiKind::Void => None,
      other => Some(self.basic_type(other)),
    };
    let params: Vec<DIType<'ctx>> = param_types.iter().copied().map(|k| self.basic_type(k)).collect();

    let subroutine_type = self
      .builder
      .create_subroutine_type(di_file, return_ty, &params, 0);

    let sp = self.builder.create_function(
      di_file.as_debug_info_scope(),
      name,
      None,
      di_file,
      line,
      subroutine_type,
      true,
      true,
      line,
      0,
      false,
    );
    function.set_subprogram(sp);

    sp
  }

  pub(crate) fn location(
    &self,
    context: &'ctx Context,
    line: u32,
    col: u32,
    scope: DISubprogram<'ctx>,
  ) -> DILocation<'ctx> {
    self
      .builder
      .create_debug_location(context, line, col, scope.as_debug_info_scope(), None)
  }
  pub(crate) fn declare_parameter(
    &self,
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    scope: DISubprogram<'ctx>,
    file: DIFile<'ctx>,
    line: u32,
    col: u32,
    name: &str,
    arg_no: u32,
    ty: DIType<'ctx>,
    slot: PointerValue<'ctx>,
  ) -> DILocalVariable<'ctx> {
    let var = self.builder.create_parameter_variable(
      scope.as_debug_info_scope(),
      name,
      arg_no,
      file,
      line,
      ty,
      true,
      0,
    );

    self.insert_declare(context, builder, slot, var, scope, line, col);
    var
  }

  pub(crate) fn declare_local(
    &self,
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    scope: DISubprogram<'ctx>,
    file: DIFile<'ctx>,
    line: u32,
    col: u32,
    name: &str,
    ty: DIType<'ctx>,
    slot: PointerValue<'ctx>,
  ) -> DILocalVariable<'ctx> {
    let var = self.builder.create_auto_variable(
      scope.as_debug_info_scope(),
      name,
      file,
      line,
      ty,
      true,
      0,
      0,
    );

    self.insert_declare(context, builder, slot, var, scope, line, col);
    var
  }

  pub(crate) fn declare_global_var(
    &mut self,
    context: &'ctx Context,
    program: &Program,
    file: FileId,
    offset: u32,
    name: &str,
    linkage_name: &str,
    kind: TsAbiKind,
    global: GlobalValue<'ctx>,
    is_local_to_unit: bool,
  ) {
    let di_file = self.file(program, file);
    let (line, _col) = line_col(program, file, offset);
    let ty = self.basic_type(kind);

    let gv_expr = self.builder.create_global_variable_expression(
      self.compile_unit.as_debug_info_scope(),
      name,
      linkage_name,
      di_file,
      line,
      ty,
      is_local_to_unit,
      None,
      None,
      0,
    );

    let dbg_kind_id = context.get_kind_id("dbg");
    global.set_metadata(gv_expr.as_metadata_value(context), dbg_kind_id);
  }

  fn insert_declare(
    &self,
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    slot: PointerValue<'ctx>,
    var: DILocalVariable<'ctx>,
    scope: DISubprogram<'ctx>,
    line: u32,
    col: u32,
  ) {
    let expr: DIExpression<'ctx> = self.builder.create_expression(Vec::new());
    let loc = self.location(context, line, col, scope);
    if let Some(bb) = builder.get_insert_block() {
      self
        .builder
        .insert_declare_at_end(slot, Some(var), Some(expr), loc, bb);
      // `insert_*_at_end` does not update the IR builder's insertion point; re-position at the end
      // of the current block so future IR continues after the newly inserted debug intrinsic.
      builder.position_at_end(bb);
    }
  }

  pub(crate) fn insert_value(
    &self,
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    module: &Module<'ctx>,
    scope: DISubprogram<'ctx>,
    var: DILocalVariable<'ctx>,
    value: BasicValueEnum<'ctx>,
    line: u32,
    col: u32,
  ) {
    let expr: DIExpression<'ctx> = self.builder.create_expression(Vec::new());
    let loc = self.location(context, line, col, scope);
    if let Some(bb) = builder.get_insert_block() {
      unsafe {
        // Declare the `llvm.dbg.value` intrinsic (if not already present). This uses the "generic"
        // signature:
        //
        //   declare void @llvm.dbg.value(metadata, metadata, metadata)
        //
        // The first operand ("metadata <value>") is encoded by wrapping a normal SSA value via
        // `LLVMValueAsMetadata` + `LLVMMetadataAsValue`.
        let module_ref = module.as_mut_ptr();
        let llvm_ctx = LLVMGetModuleContext(module_ref);

        let mut arg_tys: [LLVMTypeRef; 3] = [
          LLVMMetadataTypeInContext(llvm_ctx),
          LLVMMetadataTypeInContext(llvm_ctx),
          LLVMMetadataTypeInContext(llvm_ctx),
        ];
        let fn_ty = LLVMFunctionType(LLVMVoidTypeInContext(llvm_ctx), arg_tys.as_mut_ptr(), 3, 0);

        let name = CString::new("llvm.dbg.value").expect("llvm.dbg.value contains NUL");
        let mut func: LLVMValueRef = LLVMGetNamedFunction(module_ref, name.as_ptr());
        if func.is_null() {
          func = LLVMAddFunction(module_ref, name.as_ptr(), fn_ty);
        }

        let val_md = LLVMValueAsMetadata(value.as_value_ref());
        // `inkwell`'s debug-info wrapper types currently do not expose the underlying
        // `LLVMMetadataRef`. We still need the raw metadata pointers to build `llvm.dbg.value`
        // operands, so we rely on their layout being a transparent wrapper over `LLVMMetadataRef`.
        //
        // This matches how `DebugInfoBuilder::insert_declare_at_end` is implemented internally.
        let var_md: LLVMMetadataRef = std::mem::transmute(var);
        let expr_md: LLVMMetadataRef = std::mem::transmute(expr);
        let loc_md: LLVMMetadataRef = std::mem::transmute(loc);
        let args: [LLVMValueRef; 3] = [
          LLVMMetadataAsValue(llvm_ctx, val_md),
          LLVMMetadataAsValue(llvm_ctx, var_md),
          LLVMMetadataAsValue(llvm_ctx, expr_md),
        ];

        LLVMSetCurrentDebugLocation2(builder.as_mut_ptr(), loc_md);
        LLVMBuildCall2(
          builder.as_mut_ptr(),
          fn_ty,
          func,
          args.as_ptr() as *mut LLVMValueRef,
          args.len() as c_uint,
          b"\0".as_ptr().cast(),
        );
      }

      // Ensure the builder stays positioned in the current block; `LLVMSetCurrentDebugLocation2`
      // does not affect insertion, but the debug value emission above uses the raw LLVM builder API.
      builder.position_at_end(bb);
    }
  }
}

pub(crate) fn line_col(program: &Program, file: FileId, offset: u32) -> (u32, u32) {
  let Some(text) = program.file_text(file) else {
    return (1, 1);
  };

  let mut line: u32 = 1;
  let mut col: u32 = 1;
  for (idx, b) in text.as_bytes().iter().enumerate() {
    if idx as u32 >= offset {
      break;
    }
    if *b == b'\n' {
      line += 1;
      col = 1;
    } else {
      col += 1;
    }
  }
  (line, col)
}
