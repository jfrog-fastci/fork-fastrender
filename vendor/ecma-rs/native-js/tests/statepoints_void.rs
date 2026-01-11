use llvm_sys::analysis::{LLVMVerifierFailureAction, LLVMVerifyModule};
use llvm_sys::core::*;
use native_js::gc::statepoint::StatepointEmitter;

#[test]
fn statepoint_void_call_emits_no_gc_result_and_relocates_ptrs() {
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(c"statepoints_void_test".as_ptr(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    // LLVM statepoint-based GC lowering expects GC pointers to live in a non-zero address space.
    let gc_ptr_ty = LLVMPointerType(void_ty, 1);

    // Runtime entrypoint we want to safepoint: `void @rt_gc_safepoint()`.
    let rt_ty = LLVMFunctionType(void_ty, std::ptr::null_mut(), 0, 0);
    let rt_gc_safepoint = LLVMAddFunction(module, c"rt_gc_safepoint".as_ptr(), rt_ty);

    // Test function that performs the statepointed call and relocates two live pointers.
    // `define void @test(ptr addrspace(1) %a, ptr addrspace(1) %b)`.
    let test_fn_ty = LLVMFunctionType(void_ty, [gc_ptr_ty, gc_ptr_ty].as_ptr().cast_mut(), 2, 0);
    let test_fn = LLVMAddFunction(module, c"test".as_ptr(), test_fn_ty);
    LLVMSetGC(test_fn, c"coreclr".as_ptr());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, c"entry".as_ptr());
    LLVMPositionBuilderAtEnd(builder, entry);

    let a = LLVMGetParam(test_fn, 0);
    let b = LLVMGetParam(test_fn, 1);

    let mut statepoints = StatepointEmitter::new(ctx, module, gc_ptr_ty);
    let relocated = statepoints.emit_statepoint_call_void(builder, rt_gc_safepoint, &[], &[a, b]);
    assert_eq!(relocated.len(), 2, "expected two relocated pointers");

    LLVMBuildRetVoid(builder);

    // Ensure the IR is structurally valid (helps catch intrinsic signature mistakes).
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
      !ir.contains("gc.result"),
      "void statepoint call must not emit gc.result:\n{ir}"
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
