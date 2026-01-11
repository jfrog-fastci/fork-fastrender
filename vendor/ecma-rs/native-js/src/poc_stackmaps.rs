//! Proof-of-concept LLVM18 GC statepoints + stack map emission.
//!
//! This module exists purely to prove that:
//! - We can build a module using inkwell on LLVM 18 (opaque pointers).
//! - We can emit `gc.statepoint`/`gc.result`/`gc.relocate` in the form LLVM expects.
//! - The object emitter produces a non-empty `.llvm_stackmaps` section.

use crate::NativeJsError;
use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::{
  CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::{AddressSpace, OptimizationLevel};
use llvm_sys::core::{
  LLVMAddCallSiteAttribute, LLVMBuildCall2, LLVMBuildCallWithOperandBundles, LLVMBuildRet,
  LLVMBuildStore, LLVMBuildStructGEP2, LLVMCreateOperandBundle, LLVMCreateTypeAttribute,
  LLVMDisposeOperandBundle, LLVMGetEnumAttributeKindForName,
};

/// Builds the PoC module and returns an ELF object file as bytes.
pub fn compile_poc_object() -> Result<Vec<u8>, NativeJsError> {
  init_native_target()?;

  let context = Context::create();
  let module = context.create_module("native_js_poc_stackmaps");

  build_poc_module(&context, &module)?;

  module
    .verify()
    .map_err(|e| NativeJsError::Llvm(e.to_string()))?;

  emit_object(&module)
}

fn build_poc_module<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> Result<(), NativeJsError> {
  let builder = context.create_builder();

  let i64_ty = context.i64_type();
  let i32_ty = context.i32_type();

  // Use a dedicated address space to mark GC-managed pointers.
  //
  // LLVM's statepoint verifier treats "GC pointers" as pointers in a
  // non-default address space, and `gc.relocate` requires returning such a GC
  // pointer.
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  let pair_ty = context.struct_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);

  // declare ptr addrspace(1) @rt_alloc(i64)
  let rt_alloc_ty = gc_ptr_ty.fn_type(&[i64_ty.into()], false);
  let rt_alloc = module.add_function("rt_alloc", rt_alloc_ty, None);

  // define ptr addrspace(1) @poc_make_pair(ptr addrspace(1), ptr addrspace(1)) gc "statepoint-example"
  let make_pair_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);
  let make_pair = module.add_function("poc_make_pair", make_pair_ty, None);
  make_pair.set_gc("statepoint-example");

  let entry = context.append_basic_block(make_pair, "entry");
  builder.position_at_end(entry);

  let a = make_pair
    .get_nth_param(0)
    .ok_or_else(|| NativeJsError::Llvm("poc_make_pair missing param 0".to_string()))?
    .into_pointer_value();
  let b = make_pair
    .get_nth_param(1)
    .ok_or_else(|| NativeJsError::Llvm("poc_make_pair missing param 1".to_string()))?
    .into_pointer_value();

  let statepoint = declare_gc_statepoint(module, context)?;
  let gc_result = declare_gc_result(module, context)?;
  let gc_relocate = declare_gc_relocate(module, context)?;

  // inkwell doesn't currently expose `token` as a `BasicValue`, which is
  // required to build calls to `gc.result`/`gc.relocate`. Drop down to the C
  // API for these few instructions.
  unsafe {
    use std::ffi::CString;

    let builder_ref = builder.as_mut_ptr();

    // NOTE: LLVM 18 expects the statepoint call to be formatted as:
    //   statepoint(..., num_call_args, flags, <call args...>, num_transition_args, num_deopt_args)
    //
    // Under opaque pointers, the callee operand also needs an `elementtype`
    // attribute describing the wrapped function type.
    let statepoint_args = [
      i64_ty.const_int(0, false).as_value_ref(), // ID
      i32_ty.const_int(0, false).as_value_ref(), // NumPatchBytes
      rt_alloc.as_global_value().as_pointer_value().as_value_ref(), // Callee
      i32_ty.const_int(1, false).as_value_ref(), // NumCallArgs
      i32_ty.const_int(0, false).as_value_ref(), // Flags (reserved)
      i64_ty.const_int(16, false).as_value_ref(), // call arg: allocation size
      i32_ty.const_int(0, false).as_value_ref(), // NumTransitionArgs
      i32_ty.const_int(0, false).as_value_ref(), // NumDeoptArgs
    ];

    let gc_live_inputs = [a.as_value_ref(), b.as_value_ref()];
    let gc_live_name = CString::new("gc-live").unwrap();
    let gc_live_bundle = LLVMCreateOperandBundle(
      gc_live_name.as_ptr(),
      gc_live_name.as_bytes().len(),
      gc_live_inputs.as_ptr() as *mut _,
      gc_live_inputs.len() as u32,
    );
    let bundles = [gc_live_bundle];

    let sp_token = LLVMBuildCallWithOperandBundles(
      builder_ref,
      statepoint.get_type().as_type_ref(),
      statepoint.as_value_ref(),
      statepoint_args.as_ptr() as *mut _,
      statepoint_args.len() as u32,
      bundles.as_ptr() as *mut _,
      bundles.len() as u32,
      CString::new("statepoint").unwrap().as_ptr(),
    );

    let elementtype_kind = {
      let elementtype_name = CString::new("elementtype").unwrap();
      LLVMGetEnumAttributeKindForName(
        elementtype_name.as_ptr(),
        elementtype_name.as_bytes().len(),
      )
    };
    let elementtype_attr =
      LLVMCreateTypeAttribute(context.as_ctx_ref(), elementtype_kind, rt_alloc_ty.as_type_ref());
    // The statepoint callee argument is the 3rd parameter.
    LLVMAddCallSiteAttribute(sp_token, 3, elementtype_attr);

    LLVMDisposeOperandBundle(gc_live_bundle);

    let gc_result_args = [sp_token];
    let pair_ptr = LLVMBuildCall2(
      builder_ref,
      gc_result.get_type().as_type_ref(),
      gc_result.as_value_ref(),
      gc_result_args.as_ptr() as *mut _,
      gc_result_args.len() as u32,
      CString::new("pair").unwrap().as_ptr(),
    );

    let a_relocate_args = [
      sp_token,
      i32_ty.const_int(0, false).as_value_ref(),
      i32_ty.const_int(0, false).as_value_ref(),
    ];
    let a_relocated = LLVMBuildCall2(
      builder_ref,
      gc_relocate.get_type().as_type_ref(),
      gc_relocate.as_value_ref(),
      a_relocate_args.as_ptr() as *mut _,
      a_relocate_args.len() as u32,
      CString::new("a.relocated").unwrap().as_ptr(),
    );

    let b_relocate_args = [
      sp_token,
      i32_ty.const_int(1, false).as_value_ref(),
      i32_ty.const_int(1, false).as_value_ref(),
    ];
    let b_relocated = LLVMBuildCall2(
      builder_ref,
      gc_relocate.get_type().as_type_ref(),
      gc_relocate.as_value_ref(),
      b_relocate_args.as_ptr() as *mut _,
      b_relocate_args.len() as u32,
      CString::new("b.relocated").unwrap().as_ptr(),
    );

    let field0 = LLVMBuildStructGEP2(
      builder_ref,
      pair_ty.as_type_ref(),
      pair_ptr,
      0,
      CString::new("field0").unwrap().as_ptr(),
    );
    let field1 = LLVMBuildStructGEP2(
      builder_ref,
      pair_ty.as_type_ref(),
      pair_ptr,
      1,
      CString::new("field1").unwrap().as_ptr(),
    );
    LLVMBuildStore(builder_ref, a_relocated, field0);
    LLVMBuildStore(builder_ref, b_relocated, field1);

    LLVMBuildRet(builder_ref, pair_ptr);
  }

  Ok(())
}

fn emit_object<'ctx>(module: &Module<'ctx>) -> Result<Vec<u8>, NativeJsError> {
  init_native_target()?;

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).map_err(|e| NativeJsError::Llvm(e.to_string()))?;
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();

  let tm = target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .ok_or_else(|| NativeJsError::Llvm("failed to create LLVM TargetMachine".to_string()))?;

  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let buf = tm
    .write_to_memory_buffer(module, FileType::Object)
    .map_err(|e| NativeJsError::Llvm(e.to_string()))?;
  Ok(buf.as_slice().to_vec())
}

fn init_native_target() -> Result<(), NativeJsError> {
  static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
  match INIT.get_or_init(|| Target::initialize_native(&InitializationConfig::default()).map_err(|e| e.to_string())) {
    Ok(()) => Ok(()),
    Err(msg) => Err(NativeJsError::Llvm(msg.clone())),
  }
}

fn declare_gc_statepoint<'ctx>(
  module: &Module<'ctx>,
  context: &'ctx Context,
) -> Result<inkwell::values::FunctionValue<'ctx>, NativeJsError> {
  // `gc.statepoint` is overloaded on the address space of the *callee* pointer.
  let ptr_ty = context.ptr_type(AddressSpace::default());
  let statepoint = inkwell::intrinsics::Intrinsic::find("llvm.experimental.gc.statepoint")
    .ok_or_else(|| NativeJsError::Llvm("missing intrinsic llvm.experimental.gc.statepoint".to_string()))?;
  statepoint
    .get_declaration(module, &[ptr_ty.into()])
    .ok_or_else(|| NativeJsError::Llvm("failed to get gc.statepoint declaration".to_string()))
}

fn declare_gc_result<'ctx>(
  module: &Module<'ctx>,
  context: &'ctx Context,
) -> Result<inkwell::values::FunctionValue<'ctx>, NativeJsError> {
  // `gc.result`/`gc.relocate` are overloaded on the GC pointer type, which this
  // PoC models as addrspace(1).
  let ptr_ty = context.ptr_type(AddressSpace::from(1u16));
  let gc_result = inkwell::intrinsics::Intrinsic::find("llvm.experimental.gc.result")
    .ok_or_else(|| NativeJsError::Llvm("missing intrinsic llvm.experimental.gc.result".to_string()))?;
  gc_result
    .get_declaration(module, &[ptr_ty.into()])
    .ok_or_else(|| NativeJsError::Llvm("failed to get gc.result declaration".to_string()))
}

fn declare_gc_relocate<'ctx>(
  module: &Module<'ctx>,
  context: &'ctx Context,
) -> Result<inkwell::values::FunctionValue<'ctx>, NativeJsError> {
  let ptr_ty = context.ptr_type(AddressSpace::from(1u16));
  let gc_relocate = inkwell::intrinsics::Intrinsic::find("llvm.experimental.gc.relocate")
    .ok_or_else(|| NativeJsError::Llvm("missing intrinsic llvm.experimental.gc.relocate".to_string()))?;
  gc_relocate
    .get_declaration(module, &[ptr_ty.into()])
    .ok_or_else(|| NativeJsError::Llvm("failed to get gc.relocate declaration".to_string()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use object::{Object as _, ObjectSection as _};

  #[test]
  #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
  fn emits_stackmaps_section() {
    let obj = compile_poc_object().expect("compile PoC object");

    let file = object::File::parse(&*obj).expect("parse object file");
    let stackmaps = file
      .section_by_name(".llvm_stackmaps")
      .expect("missing .llvm_stackmaps section");
    let data = stackmaps.data().expect("read .llvm_stackmaps section");
    assert!(!data.is_empty(), ".llvm_stackmaps section exists but is empty");

    assert!(
      data.len() >= 16,
      ".llvm_stackmaps section too small ({} bytes)",
      data.len()
    );
    let num_records = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    assert!(num_records > 0, "expected at least one stackmap record");
  }
}

