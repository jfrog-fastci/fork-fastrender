//! Proof-of-concept LLVM18 GC statepoints + stack map emission.
//!
//! This module exists purely to prove that:
//! - We can build a module using inkwell on LLVM 18 (opaque pointers).
//! - The object emitter produces a non-empty `.llvm_stackmaps` section.
//!
//! Unlike the low-level `gc::statepoint`/`GcFrame` unit tests (which manually emit statepoints via
//! the LLVM C API), this PoC exercises the **real native-js pipeline**:
//! - runtime calls are emitted via `RuntimeAbi::emit_runtime_call` (ABI-correct extern decls +
//!   addrspace(1) call signatures via indirect calls), and
//! - statepoints/stackmaps are produced by LLVM's `place-safepoints` +
//!   `rewrite-statepoints-for-gc` passes during object emission.

use crate::emit;
use crate::NativeJsError;
use inkwell::context::Context;
use inkwell::module::Module;

use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::OptimizationLevel;
use llvm_sys::core::{LLVMBuildStore, LLVMBuildStructGEP2};

use crate::llvm::gc;
use crate::runtime_abi::{RuntimeAbi, RuntimeFn};

/// Builds the PoC module and returns an ELF object file as bytes.
pub fn compile_poc_object() -> Result<Vec<u8>, NativeJsError> {
  let context = Context::create();
  let module = context.create_module("native_js_poc_stackmaps");

  build_poc_module(&context, &module)?;

  module
    .verify()
    .map_err(|e| NativeJsError::Llvm(e.to_string()))?;

  let obj = emit::emit_object_with_statepoints(
    &module,
    emit::TargetConfig {
      opt_level: OptimizationLevel::None,
      ..emit::TargetConfig::default()
    },
  )
  .map_err(|e| NativeJsError::Llvm(e.to_string()))?;

  Ok(obj)
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
  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());

  let pair_ty = context.struct_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);

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

  let rt = RuntimeAbi::new(context, module);
  let alloc_size = i64_ty.const_int(16, false);
  let shape_id = i32_ty.const_int(1, false);
  let call = rt
    .emit_runtime_call(
      &builder,
      RuntimeFn::Alloc,
      &[alloc_size.into(), shape_id.into()],
      "pair",
    )
    .map_err(|e| NativeJsError::Llvm(e.to_string()))?;
  let pair_ptr = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns a GC pointer")
    .into_pointer_value();

  // The GEP helpers require an element type in opaque-pointer mode; use the LLVM C API.
  unsafe {
    use std::ffi::CString;

    let builder_ref = builder.as_mut_ptr();
    let field0 = LLVMBuildStructGEP2(
      builder_ref,
      pair_ty.as_type_ref(),
      pair_ptr.as_value_ref(),
      0,
      CString::new("field0").unwrap().as_ptr(),
    );
    let field1 = LLVMBuildStructGEP2(
      builder_ref,
      pair_ty.as_type_ref(),
      pair_ptr.as_value_ref(),
      1,
      CString::new("field1").unwrap().as_ptr(),
    );
    LLVMBuildStore(builder_ref, a.as_value_ref(), field0);
    LLVMBuildStore(builder_ref, b.as_value_ref(), field1);
  }

  builder
    .build_return(Some(&pair_ptr))
    .expect("build return");

  Ok(())
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
