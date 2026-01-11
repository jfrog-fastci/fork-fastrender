use crate::gc::roots::GcFrame;
use crate::gc::statepoint::StatepointEmitter;
use llvm_sys::analysis::{LLVMVerifyModule, LLVMVerifierFailureAction};
use llvm_sys::core::{
  LLVMAppendBasicBlockInContext, LLVMBuildRetVoid, LLVMConstNull, LLVMContextCreate,
  LLVMContextDispose, LLVMCreateBuilderInContext, LLVMDisposeBuilder, LLVMDisposeMessage,
  LLVMDisposeModule, LLVMFunctionType, LLVMGetNamedFunction, LLVMModuleCreateWithNameInContext,
  LLVMPositionBuilderAtEnd, LLVMPrintModuleToString, LLVMSetGC, LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMModuleRef, LLVMValueRef};
use std::ffi::{CStr, CString};
use std::ptr;

/// Build a tiny function that:
///   - allocates two rooted GC slots in the entry block,
///   - emits a safepointed call, and
///   - writes back `gc.relocate` results into the slots.
///
/// This is used by IR-level tests to lock down the expected relocation
/// writeback pattern.
pub fn demo_gc_root_slots_ir() -> String {
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(b"demo\0".as_ptr().cast(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    // Declare `void @callee()`.
    let void_ty = LLVMVoidTypeInContext(ctx);
    let callee_fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
    let callee = llvm_get_or_add_fn(module, "callee", callee_fn_ty);

    // Define `void @test() gc "statepoint-example"`.
    let test_fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
    let test_fn = llvm_get_or_add_fn(module, "test", test_fn_ty);
    LLVMSetGC(test_fn, b"statepoint-example\0".as_ptr().cast());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, b"entry\0".as_ptr().cast());
    LLVMPositionBuilderAtEnd(builder, entry);

    let frame = GcFrame::new(ctx, entry);
    let gc_ptr_ty = frame.gc_ptr_ty();
    let null_gc = LLVMConstNull(gc_ptr_ty);

    let mut sp = StatepointEmitter::new(ctx, module, gc_ptr_ty);

    frame.alloc_slot(builder, null_gc);
    frame.alloc_slot(builder, null_gc);

    frame.safepoint_call(builder, &mut sp, callee, &[]);

    LLVMBuildRetVoid(builder);

    verify_module(module);

    let c_str = LLVMPrintModuleToString(module);
    let ir = CStr::from_ptr(c_str).to_string_lossy().into_owned();
    LLVMDisposeMessage(c_str);

    LLVMDisposeBuilder(builder);
    LLVMDisposeModule(module);
    LLVMContextDispose(ctx);

    ir
  }
}

unsafe fn llvm_get_or_add_fn(module: LLVMModuleRef, name: &str, ty: llvm_sys::prelude::LLVMTypeRef) -> LLVMValueRef {
  let name_c = CString::new(name).unwrap();
  let existing = LLVMGetNamedFunction(module, name_c.as_ptr());
  if !existing.is_null() {
    return existing;
  }
  llvm_sys::core::LLVMAddFunction(module, name_c.as_ptr(), ty)
}

unsafe fn verify_module(module: LLVMModuleRef) {
  let mut err: *mut i8 = ptr::null_mut();
  let failed = LLVMVerifyModule(
    module,
    LLVMVerifierFailureAction::LLVMReturnStatusAction,
    &mut err,
  );
  if failed != 0 {
    let msg = if err.is_null() {
      "<unknown LLVM verification error>".into()
    } else {
      let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
      LLVMDisposeMessage(err);
      msg
    };
    panic!("LLVM module verification failed: {msg}");
  }
}
