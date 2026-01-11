//! Proof-of-concept LLVM18 GC statepoints + stack map emission.
//!
//! This module exists purely to prove that:
//! - We can build a module using inkwell on LLVM 18 (opaque pointers).
//! - We can emit `gc.statepoint`/`gc.result`/`gc.relocate` in the form LLVM expects.
//! - The object emitter produces a non-empty `.llvm_stackmaps` section.
//!
//! As part of GC bring-up, this PoC also exercises the "root slot" strategy:
//! GC locals are modeled as `alloca ptr addrspace(1)` slots, and `gc.relocate`
//! values are written back into those slots at every safepoint.

use crate::gc::roots::GcFrame;
use crate::gc::statepoint::StatepointEmitter;
use crate::NativeJsError;
use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::{AddressSpace, OptimizationLevel};
use llvm_sys::core::{LLVMBuildRet, LLVMBuildStore, LLVMBuildStructGEP2, LLVMGetInsertBlock};

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

fn build_poc_module<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
) -> Result<(), NativeJsError> {
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

  // declare ptr addrspace(1) @rt_alloc(i64, i32)
  let rt_alloc_ty = gc_ptr_ty.fn_type(&[i64_ty.into(), i32_ty.into()], false);
  let rt_alloc = module.add_function("rt_alloc", rt_alloc_ty, None);

  // define ptr addrspace(1) @poc_make_pair(ptr addrspace(1), ptr addrspace(1)) gc "coreclr"
  let make_pair_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);
  let make_pair = module.add_function("poc_make_pair", make_pair_ty, None);
  crate::stack_walking::apply_stack_walking_attrs(context, make_pair);

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

  // inkwell doesn't currently expose `token` as a `BasicValue`, which is
  // required to build calls to `gc.result`/`gc.relocate`. Drop down to the C
  // API for these few instructions.
  unsafe {
    use std::ffi::CString;

    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new(context.as_ctx_ref(), entry_block);

    let a_slot = frame.alloc_slot(builder_ref, a.as_value_ref());
    let b_slot = frame.alloc_slot(builder_ref, b.as_value_ref());

    let mut statepoints = StatepointEmitter::new(
      context.as_ctx_ref(),
      module.as_mut_ptr(),
      gc_ptr_ty.as_type_ref(),
    );

    let alloc_size = i64_ty.const_int(16, false).as_value_ref();
    let shape_id = i32_ty.const_int(1, false).as_value_ref();
    let pair_ptr = frame
      .safepoint_call(
        builder_ref,
        &mut statepoints,
        rt_alloc.as_value_ref(),
        &[alloc_size, shape_id],
      )
      .expect("rt_alloc returns a GC pointer so gc.result must exist");

    let a_relocated = frame.load(builder_ref, a_slot, "a.relocated");
    let b_relocated = frame.load(builder_ref, b_slot, "b.relocated");

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
  crate::llvm::init_native_target().map_err(NativeJsError::Llvm)
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
    assert!(
      !data.is_empty(),
      ".llvm_stackmaps section exists but is empty"
    );

    assert!(
      data.len() >= 16,
      ".llvm_stackmaps section too small ({} bytes)",
      data.len()
    );
    let num_records = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    assert!(num_records > 0, "expected at least one stackmap record");
  }
}
