use crate::gc::roots::GcFrame;
use crate::gc::statepoint::StatepointEmitter;
use llvm_sys::analysis::{LLVMVerifyModule, LLVMVerifierFailureAction};
use llvm_sys::core::{
  LLVMAppendBasicBlockInContext, LLVMBuildGEP2, LLVMBuildRetVoid, LLVMConstInt, LLVMConstNull,
  LLVMContextCreate, LLVMContextDispose, LLVMCreateBuilderInContext, LLVMDisposeBuilder,
  LLVMDisposeMessage, LLVMDisposeModule, LLVMFunctionType, LLVMGetNamedFunction, LLVMGetParam,
  LLVMInt8TypeInContext, LLVMInt64TypeInContext, LLVMPointerType, LLVMModuleCreateWithNameInContext,
  LLVMPositionBuilderAtEnd, LLVMPrintModuleToString, LLVMSetGC, LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMModuleRef, LLVMValueRef};
use std::ffi::{CStr, CString};
use std::ptr;

use crate::llvm::gc::GC_STRATEGY;

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

    // Define `void @test() gc "coreclr"` (see `native-js/docs/llvm_gc_strategy.md`).
    let test_fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
    let test_fn = llvm_get_or_add_fn(module, "test", test_fn_ty);
    let gc_name = CString::new(GC_STRATEGY).unwrap();
    LLVMSetGC(test_fn, gc_name.as_ptr());

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

/// Like [`demo_gc_root_slots_ir`], but uses an *indirect* function pointer callee.
///
/// This locks in that our manual statepoint emission layer can attach the required
/// `elementtype(<fn-ty>)` attribute even when the callee is a runtime `ptr` value.
pub fn demo_gc_root_slots_indirect_call_ir() -> String {
  unsafe {
    let ctx = LLVMContextCreate();
    let module = LLVMModuleCreateWithNameInContext(b"demo_indirect\0".as_ptr().cast(), ctx);
    let builder = LLVMCreateBuilderInContext(ctx);

    let void_ty = LLVMVoidTypeInContext(ctx);
    let fp_ptr_ty = LLVMPointerType(void_ty, 0);
    let gc_ptr_ty = LLVMPointerType(void_ty, 1);

    // define void @test_indirect(ptr %fp, ptr addrspace(1) %obj) gc "coreclr"
    let mut params = [fp_ptr_ty, gc_ptr_ty];
    let test_fn_ty = LLVMFunctionType(void_ty, params.as_mut_ptr(), params.len() as u32, 0);
    let test_fn = llvm_get_or_add_fn(module, "test_indirect", test_fn_ty);
    let gc_name = CString::new(GC_STRATEGY).unwrap();
    LLVMSetGC(test_fn, gc_name.as_ptr());

    let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, b"entry\0".as_ptr().cast());
    LLVMPositionBuilderAtEnd(builder, entry);

    let fp = LLVMGetParam(test_fn, 0);
    let obj = LLVMGetParam(test_fn, 1);

    let frame = GcFrame::new(ctx, entry);
    let mut sp = StatepointEmitter::new(ctx, module, gc_ptr_ty);

    // Root %obj via a slot so the statepoint uses `"gc-live"` + relocate writeback.
    frame.alloc_slot(builder, obj);

    // The callsite signature must be provided for indirect callees under opaque pointers.
    let callee_fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
    frame.safepoint_call_indirect(builder, &mut sp, fp, callee_fn_ty, &[]);

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

/// Build a tiny function that roots:
///   - a base GC pointer, and
///   - a derived/interior pointer (`gep` from the base),
/// and then emits a safepointed call.
///
/// Used by IR-level tests to lock down base+derived relocation indices.
pub fn demo_gc_root_derived_ptr_ir() -> String {
  unsafe { demo_gc_root_derived_ptrs_ir(1) }
}

/// Like [`demo_gc_root_derived_ptr_ir`], but roots two derived pointers sharing the same base.
pub fn demo_gc_root_multi_derived_ptr_ir() -> String {
  unsafe { demo_gc_root_derived_ptrs_ir(2) }
}

unsafe fn demo_gc_root_derived_ptrs_ir(num_derived: u64) -> String {
  let ctx = LLVMContextCreate();
  let module = LLVMModuleCreateWithNameInContext(b"demo\0".as_ptr().cast(), ctx);
  let builder = LLVMCreateBuilderInContext(ctx);

  // Declare `void @callee()`.
  let void_ty = LLVMVoidTypeInContext(ctx);
  let callee_fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
  let callee = llvm_get_or_add_fn(module, "callee", callee_fn_ty);

  // Define `void @test(ptr addrspace(1) %base) gc "coreclr"`.
  let gc_ptr_ty = LLVMPointerType(void_ty, 1);
  let test_fn_ty = LLVMFunctionType(void_ty, [gc_ptr_ty].as_ptr().cast_mut(), 1, 0);
  let test_fn = llvm_get_or_add_fn(module, "test", test_fn_ty);
  let gc_name = CString::new(GC_STRATEGY).unwrap();
  LLVMSetGC(test_fn, gc_name.as_ptr());

  let entry = LLVMAppendBasicBlockInContext(ctx, test_fn, b"entry\0".as_ptr().cast());
  LLVMPositionBuilderAtEnd(builder, entry);

  let frame = GcFrame::new(ctx, entry);
  let mut sp = StatepointEmitter::new(ctx, module, frame.gc_ptr_ty());

  let base = LLVMGetParam(test_fn, 0);
  let base_slot = frame.root_base(builder, base);

  let i8_ty = LLVMInt8TypeInContext(ctx);
  let i64_ty = LLVMInt64TypeInContext(ctx);
  for i in 0..num_derived {
    let offset = 8 + (i * 8);
    let idx = LLVMConstInt(i64_ty, offset, 0);
    let mut idxs = [idx];
    let derived = LLVMBuildGEP2(
      builder,
      i8_ty,
      base,
      idxs.as_mut_ptr(),
      idxs.len() as u32,
      CString::new(format!("derived{i}")).unwrap().as_ptr(),
    );
    frame.root_derived(builder, &base_slot, derived);
  }

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
