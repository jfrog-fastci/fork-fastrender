use llvm_sys::analysis::{LLVMVerifierFailureAction, LLVMVerifyModule};
use llvm_sys::core::*;

use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc::GC_STRATEGY;
use std::ffi::CString;

#[test]
fn statepoint_nonvoid_call_emits_gc_result_and_relocates_ptrs() {
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(c"statepoints_nonvoid_test".as_ptr(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    let gc_ptr_ty = LLVMPointerType(void_ty, 1);

    // External callee: `declare ptr addrspace(1) @identity(ptr addrspace(1))`.
    let callee_ty = LLVMFunctionType(gc_ptr_ty, [gc_ptr_ty].as_ptr().cast_mut(), 1, 0);
    let identity = LLVMAddFunction(module, c"identity".as_ptr(), callee_ty);

    // Test function: `define ptr addrspace(1) @test(ptr addrspace(1) %a, ptr addrspace(1) %b)`.
    let test_fn_ty = LLVMFunctionType(gc_ptr_ty, [gc_ptr_ty, gc_ptr_ty].as_ptr().cast_mut(), 2, 0);
    let test_fn = LLVMAddFunction(module, c"test".as_ptr(), test_fn_ty);
    let gc_name = CString::new(GC_STRATEGY).unwrap();
    LLVMSetGC(test_fn, gc_name.as_ptr());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, c"entry".as_ptr());
    LLVMPositionBuilderAtEnd(builder, entry);

    let a = LLVMGetParam(test_fn, 0);
    let b = LLVMGetParam(test_fn, 1);

    let mut statepoints = StatepointEmitter::new(ctx, module, gc_ptr_ty);
    let sp = statepoints.emit_statepoint_call(builder, identity, &[a], &[a, b]);
    LLVMBuildRet(builder, sp.result.expect("expected non-void result"));

    // Verify the module (ensures statepoint shape + overloads are correct).
    let mut err = std::ptr::null_mut();
    let ok = LLVMVerifyModule(module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut err);
    if ok != 0 {
      let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
      LLVMDisposeMessage(err);
      panic!("LLVM module verification failed:\n{msg}");
    }

    let ir_ptr = LLVMPrintModuleToString(module);
    let ir = std::ffi::CStr::from_ptr(ir_ptr).to_string_lossy().into_owned();
    LLVMDisposeMessage(ir_ptr);

    assert!(
      ir.contains("gc.statepoint"),
      "expected gc.statepoint intrinsic call in IR:\n{ir}"
    );
    assert!(
      ir.contains("gc.result"),
      "non-void statepoint call must emit gc.result:\n{ir}"
    );
    assert!(
      ir.contains("gc.relocate") && ir.contains("i32 0, i32 0") && ir.contains("i32 1, i32 1"),
      "expected gc.relocate calls for each live pointer:\n{ir}"
    );

    LLVMDisposeBuilder(builder);
    LLVMDisposeModule(module);
    LLVMContextDispose(ctx);
  }
}

#[test]
fn statepoint_indirect_nonvoid_call_emits_elementtype_and_gc_result() {
  // LLVM 18 opaque pointers require `gc.statepoint`'s callee operand to carry
  // `elementtype(<fn-ty>)`. This must also hold for *indirect* callees that return
  // a value (so we additionally exercise `gc.result.*`).
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(c"statepoints_nonvoid_indirect_test".as_ptr(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    let gc_ptr_ty = LLVMPointerType(void_ty, 1);
    let fp_ty = LLVMPointerType(void_ty, 0);

    // Signature of the indirect callee: `ptr addrspace(1) (ptr addrspace(1))`.
    let callee_fn_ty = LLVMFunctionType(gc_ptr_ty, [gc_ptr_ty].as_ptr().cast_mut(), 1, 0);
    let _identity = LLVMAddFunction(module, c"identity".as_ptr(), callee_fn_ty);

    // define ptr addrspace(1) @test(ptr %fp, ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr"
    let test_fn_ty =
      LLVMFunctionType(gc_ptr_ty, [fp_ty, gc_ptr_ty, gc_ptr_ty].as_ptr().cast_mut(), 3, 0);
    let test_fn = LLVMAddFunction(module, c"test_indirect".as_ptr(), test_fn_ty);
    let gc_name = CString::new(GC_STRATEGY).unwrap();
    LLVMSetGC(test_fn, gc_name.as_ptr());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, c"entry".as_ptr());
    LLVMPositionBuilderAtEnd(builder, entry);

    let fp = LLVMGetParam(test_fn, 0);
    let a = LLVMGetParam(test_fn, 1);
    let b = LLVMGetParam(test_fn, 2);

    let mut statepoints = StatepointEmitter::new(ctx, module, gc_ptr_ty);
    let base_indices = [0u32, 1u32];
    let sp = statepoints.emit_statepoint_call_indirect(
      builder,
      fp,
      callee_fn_ty,
      &[a],
      &[a, b],
      &base_indices,
    );
    LLVMBuildRet(builder, sp.result.expect("expected non-void result"));

    // Ensure the IR is verifier-correct.
    let mut err = std::ptr::null_mut();
    let ok = LLVMVerifyModule(module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut err);
    if ok != 0 {
      let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
      LLVMDisposeMessage(err);
      panic!("LLVM module verification failed:\n{msg}");
    }

    let ir_ptr = LLVMPrintModuleToString(module);
    let ir = std::ffi::CStr::from_ptr(ir_ptr).to_string_lossy().into_owned();
    LLVMDisposeMessage(ir_ptr);

    assert!(
      ir.contains("@llvm.experimental.gc.statepoint.p0"),
      "expected gc.statepoint intrinsic in IR:\n{ir}"
    );
    assert!(
      ir.contains("ptr elementtype(ptr addrspace(1) (ptr addrspace(1))) %"),
      "expected statepoint callee operand to carry elementtype for indirect call:\n{ir}"
    );
    assert!(
      ir.contains("@llvm.experimental.gc.result.p1"),
      "expected gc.result.p1 for non-void indirect call:\n{ir}"
    );

    LLVMDisposeBuilder(builder);
    LLVMDisposeModule(module);
    LLVMContextDispose(ctx);
  }
}
