use llvm_sys::core::{
  LLVMAddCallSiteAttribute, LLVMAddFunction, LLVMBuildCall2, LLVMBuildCallWithOperandBundles,
  LLVMConstInt, LLVMCreateOperandBundle, LLVMCreateTypeAttribute, LLVMDisposeOperandBundle,
  LLVMFunctionType, LLVMGetEnumAttributeKindForName,
  LLVMGetNamedFunction, LLVMGetPointerAddressSpace, LLVMGetReturnType, LLVMGetTypeKind,
  LLVMGlobalGetValueType, LLVMIntTypeInContext, LLVMPointerType, LLVMTokenTypeInContext,
  LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{
  LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMOperandBundleRef, LLVMTypeRef, LLVMValueRef,
};
use llvm_sys::{LLVMCallConv, LLVMTypeKind};
use std::ffi::CString;

/// Result of emitting a `gc.statepoint` + `gc.relocate` sequence.
pub struct StatepointCall {
  pub token: LLVMValueRef,
  pub result: Option<LLVMValueRef>,
  /// One relocated pointer per input in the original `gc_live` list.
  pub relocated: Vec<LLVMValueRef>,
}

/// Minimal helper for emitting LLVM statepoint intrinsics.
///
/// This is intentionally narrow for the PoC: it supports statepoints with no
/// deopt args, and represents GC live pointers via the `"gc-live"` operand
/// bundle (preferred for opaque pointers).
pub struct StatepointEmitter {
  ctx: LLVMContextRef,
  module: LLVMModuleRef,
  statepoint_fn: LLVMValueRef,
  statepoint_fn_ty: LLVMTypeRef,
  gc_relocate_fn: LLVMValueRef,
  gc_relocate_fn_ty: LLVMTypeRef,
  elementtype_attr_kind: u32,
  i32_ty: LLVMTypeRef,
  i64_ty: LLVMTypeRef,
  token_ty: LLVMTypeRef,
}

impl StatepointEmitter {
  pub unsafe fn new(ctx: LLVMContextRef, module: LLVMModuleRef, gc_ptr_ty: LLVMTypeRef) -> Self {
    let token_ty = LLVMTokenTypeInContext(ctx);
    let i32_ty = LLVMIntTypeInContext(ctx, 32);
    let i64_ty = LLVMIntTypeInContext(ctx, 64);
    let callee_ptr_ty = LLVMPointerType(LLVMVoidTypeInContext(ctx), 0);

    let statepoint_fn_ty = LLVMFunctionType(
      token_ty,
      [i64_ty, i32_ty, callee_ptr_ty, i32_ty, i32_ty].as_mut_ptr(),
      5,
      1,
    );
    let statepoint_fn = get_or_declare_fn(
      module,
      "llvm.experimental.gc.statepoint.p0",
      statepoint_fn_ty,
    );

    let gc_relocate_fn_ty =
      LLVMFunctionType(gc_ptr_ty, [token_ty, i32_ty, i32_ty].as_mut_ptr(), 3, 0);
    let gc_relocate_fn = get_or_declare_fn(
      module,
      &format!(
        "llvm.experimental.gc.relocate.p{}",
        LLVMGetPointerAddressSpace(gc_ptr_ty)
      ),
      gc_relocate_fn_ty,
    );

    let elementtype_attr_kind =
      LLVMGetEnumAttributeKindForName("elementtype\0".as_ptr().cast(), "elementtype".len());

    Self {
      ctx,
      module,
      statepoint_fn,
      statepoint_fn_ty,
      gc_relocate_fn,
      gc_relocate_fn_ty,
      elementtype_attr_kind,
      i32_ty,
      i64_ty,
      token_ty,
    }
  }

  /// Emit `gc.statepoint` wrapping a call to `callee` with `call_args`.
  ///
  /// `gc_live` values are surfaced via a `"gc-live"` operand bundle.
  pub unsafe fn emit_statepoint_call(
    &mut self,
    builder: LLVMBuilderRef,
    callee: LLVMValueRef,
    call_args: &[LLVMValueRef],
    gc_live: &[LLVMValueRef],
  ) -> StatepointCall {
    let callee_fn_ty = LLVMGlobalGetValueType(callee);
    let callee_ret_ty = LLVMGetReturnType(callee_fn_ty);
    let callee_ret_kind = LLVMGetTypeKind(callee_ret_ty);

    // `gc.statepoint` argument layout:
    //   (id, patch_bytes, callee, num_call_args, flags,
    //    call_args...,
    //    num_deopt_args, deopt_args...,
    //    num_gc_args, gc_args...)
    //
    // In the PoC we emit no deopt args and carry live pointers via the
    // `"gc-live"` operand bundle, so `num_gc_args` is always 0 and `gc_args` is
    // empty.
    let mut sp_args = Vec::with_capacity(5 + call_args.len() + 2);
    sp_args.push(LLVMConstInt(self.i64_ty, 0, 0));
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0));
    sp_args.push(callee);
    sp_args.push(LLVMConstInt(self.i32_ty, call_args.len() as u64, 0));
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0)); // flags
    sp_args.extend_from_slice(call_args);
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0)); // num_deopt_args
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0)); // num_gc_args

    // Attach `elementtype(...)` to the callee operand (required under opaque pointers).
    let elementtype_attr = LLVMCreateTypeAttribute(self.ctx, self.elementtype_attr_kind, callee_fn_ty);

    let mut bundles: Vec<LLVMOperandBundleRef> = Vec::new();
    if !gc_live.is_empty() {
      // `gc-live` operand bundle.
      let name = CString::new("gc-live").unwrap();
      let bundle = LLVMCreateOperandBundle(
        name.as_ptr(),
        name.as_bytes().len(),
        gc_live.as_ptr().cast_mut(),
        gc_live.len() as u32,
      );
      bundles.push(bundle);
    }

    let token = LLVMBuildCallWithOperandBundles(
      builder,
      self.statepoint_fn_ty,
      self.statepoint_fn,
      sp_args.as_mut_ptr(),
      sp_args.len() as u32,
      bundles.as_mut_ptr(),
      bundles.len() as u32,
      b"statepoint_token\0".as_ptr().cast(),
    );
    LLVMAddCallSiteAttribute(token, 3, elementtype_attr);

    for bundle in bundles {
      LLVMDisposeOperandBundle(bundle);
    }

    let result = if callee_ret_kind == LLVMTypeKind::LLVMVoidTypeKind {
      None
    } else {
      let gc_result_fn = get_or_declare_fn(
        self.module,
        &gc_result_intrinsic_name(callee_ret_ty),
        LLVMFunctionType(callee_ret_ty, [self.token_ty].as_mut_ptr(), 1, 0),
      );

      Some(LLVMBuildCall2(
        builder,
        LLVMFunctionType(callee_ret_ty, [self.token_ty].as_mut_ptr(), 1, 0),
        gc_result_fn,
        [token].as_mut_ptr(),
        1,
        b"gc_result\0".as_ptr().cast(),
      ))
    };

    let mut relocated = Vec::with_capacity(gc_live.len());
    for (idx, _) in gc_live.iter().enumerate() {
      let idx_const = LLVMConstInt(self.i32_ty, idx as u64, 0);
      let relocate = LLVMBuildCall2(
        builder,
        self.gc_relocate_fn_ty,
        self.gc_relocate_fn,
        [token, idx_const, idx_const].as_mut_ptr(),
        3,
        CString::new(format!("gc_relocate{idx}")).unwrap().as_ptr(),
      );
      llvm_sys::core::LLVMSetInstructionCallConv(relocate, LLVMCallConv::LLVMColdCallConv as u32);
      relocated.push(relocate);
    }

    StatepointCall {
      token,
      result,
      relocated,
    }
  }
}

unsafe fn get_or_declare_fn(module: LLVMModuleRef, name: &str, ty: LLVMTypeRef) -> LLVMValueRef {
  let name = CString::new(name).unwrap();
  let existing = LLVMGetNamedFunction(module, name.as_ptr());
  if !existing.is_null() {
    return existing;
  }
  LLVMAddFunction(module, name.as_ptr(), ty)
}

unsafe fn gc_result_intrinsic_name(ret_ty: LLVMTypeRef) -> String {
  match LLVMGetTypeKind(ret_ty) {
    LLVMTypeKind::LLVMPointerTypeKind => {
      let aspace = LLVMGetPointerAddressSpace(ret_ty);
      format!("llvm.experimental.gc.result.p{aspace}")
    }
    LLVMTypeKind::LLVMIntegerTypeKind => {
      let bits = llvm_sys::core::LLVMGetIntTypeWidth(ret_ty);
      format!("llvm.experimental.gc.result.i{bits}")
    }
    other => panic!("unsupported gc.result return type kind: {other:?}"),
  }
}
