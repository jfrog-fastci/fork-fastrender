use llvm_sys::analysis::{LLVMVerifierFailureAction, LLVMVerifyModule};
use llvm_sys::core::*;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc::GC_STRATEGY;
use std::ffi::CString;

#[test]
fn gc_pointer_call_args_are_included_in_gc_live_bundle() {
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(c"statepoint_call_args_are_roots".as_ptr(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    let gc_ptr_ty = LLVMPointerType(void_ty, 1);

    // declare void @callee(ptr addrspace(1))
    let callee_ty = LLVMFunctionType(void_ty, [gc_ptr_ty].as_ptr().cast_mut(), 1, 0);
    let callee = LLVMAddFunction(module, c"callee".as_ptr(), callee_ty);

    // define void @test(ptr addrspace(1) %p) gc "coreclr"
    let test_ty = LLVMFunctionType(void_ty, [gc_ptr_ty].as_ptr().cast_mut(), 1, 0);
    let test_fn = LLVMAddFunction(module, c"test".as_ptr(), test_ty);
    let gc_name = CString::new(GC_STRATEGY).unwrap();
    LLVMSetGC(test_fn, gc_name.as_ptr());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, c"entry".as_ptr());
    LLVMPositionBuilderAtEnd(builder, entry);

    let p = LLVMGetParam(test_fn, 0);

    let mut statepoints = StatepointEmitter::new(ctx, module, gc_ptr_ty);
    // NOTE: `p` is *only* passed as a call arg, not as an explicit `gc_live` root.
    let sp = statepoints.emit_statepoint_call(builder, callee, &[p], &[]);
    assert!(
      sp.relocated.is_empty(),
      "relocated list corresponds to explicit gc_live inputs; call args are only auto-rooted"
    );

    LLVMBuildRetVoid(builder);

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
      ir.contains("\"gc-live\"(ptr addrspace(1)"),
      "expected call-arg GC pointer to be auto-added to gc-live bundle:\n{ir}"
    );
    assert!(
      ir.contains("@llvm.experimental.gc.relocate.p1"),
      "expected at least one gc.relocate so the pointer is recorded in the stackmap:\n{ir}"
    );

    LLVMDisposeBuilder(builder);
    LLVMDisposeModule(module);
    LLVMContextDispose(ctx);
  }
}
