use llvm_sys::core::{
  LLVMAddFunction, LLVMAppendBasicBlockInContext, LLVMBuildCall2, LLVMBuildRetVoid, LLVMContextCreate, LLVMContextDispose,
  LLVMCreateBuilderInContext, LLVMDisposeBuilder, LLVMDisposeMessage, LLVMDisposeModule, LLVMFunctionType,
  LLVMModuleCreateWithNameInContext, LLVMPositionBuilderAtEnd, LLVMPrintModuleToString, LLVMSetGC, LLVMVoidTypeInContext,
};
use llvm_sys::transforms::pass_builder::{LLVMCreatePassBuilderOptions, LLVMDisposePassBuilderOptions, LLVMRunPasses};
use std::ffi::{CStr, CString};
use std::ptr;

use native_js::llvm::statepoint_directives::{
  set_callsite_statepoint_id, set_callsite_statepoint_num_patch_bytes,
};

#[test]
fn rewrite_statepoints_honors_callsite_directives() {
  unsafe {
    let ctx = LLVMContextCreate();

    let module_name = CString::new("statepoint_directives").unwrap();
    let module = LLVMModuleCreateWithNameInContext(module_name.as_ptr(), ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    let bar_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);

    let bar_name = CString::new("bar").unwrap();
    let bar = LLVMAddFunction(module, bar_name.as_ptr(), bar_ty);

    let foo_name = CString::new("foo").unwrap();
    let foo = LLVMAddFunction(module, foo_name.as_ptr(), LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0));

    let gc_name = CString::new("statepoint-example").unwrap();
    LLVMSetGC(foo, gc_name.as_ptr());

    let entry_name = CString::new("entry").unwrap();
    let entry = LLVMAppendBasicBlockInContext(ctx, foo, entry_name.as_ptr());

    let builder = LLVMCreateBuilderInContext(ctx);
    LLVMPositionBuilderAtEnd(builder, entry);

    let call_name = CString::new("").unwrap();
    let call = LLVMBuildCall2(builder, bar_ty, bar, ptr::null_mut(), 0, call_name.as_ptr());
    set_callsite_statepoint_id(call, 42);
    set_callsite_statepoint_num_patch_bytes(call, 16);

    LLVMBuildRetVoid(builder);
    LLVMDisposeBuilder(builder);

    let passes = CString::new("rewrite-statepoints-for-gc").unwrap();
    let opts = LLVMCreatePassBuilderOptions();
    let err = LLVMRunPasses(module, passes.as_ptr(), ptr::null_mut(), opts);
    LLVMDisposePassBuilderOptions(opts);

    if !err.is_null() {
      panic!("LLVMRunPasses failed (non-null LLVMErrorRef)");
    }

    let ir_ptr = LLVMPrintModuleToString(module);
    let ir = CStr::from_ptr(ir_ptr).to_string_lossy().into_owned();
    LLVMDisposeMessage(ir_ptr);

    assert!(
      ir.contains("@llvm.experimental.gc.statepoint.p0(i64 42, i32 16"),
      "expected statepoint id/patch-bytes in rewritten IR, got:\n{ir}"
    );

    LLVMDisposeModule(module);
    LLVMContextDispose(ctx);
  }
}
