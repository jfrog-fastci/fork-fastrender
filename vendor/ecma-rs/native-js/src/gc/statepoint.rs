use llvm_sys::core::{
  LLVMAddCallSiteAttribute, LLVMBuildCall2, LLVMBuildCallWithOperandBundles, LLVMConstInt,
  LLVMCreateOperandBundle, LLVMCreateTypeAttribute, LLVMDisposeOperandBundle,
  LLVMGetEnumAttributeKindForName, LLVMGetIntrinsicDeclaration, LLVMGetPointerAddressSpace,
  LLVMGetReturnType, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIntTypeInContext,
  LLVMLookupIntrinsicID, LLVMTypeOf,
};
use llvm_sys::prelude::{
  LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMOperandBundleRef, LLVMTypeRef, LLVMValueRef,
};
use llvm_sys::{LLVMCallConv, LLVMTypeKind};
use std::collections::HashMap;
use std::ffi::CString;

use crate::llvm::gc::GC_ADDR_SPACE;

/// Default `gc.statepoint` patchpoint ID (`patchpoint_id` in `.llvm_stackmaps`).
///
/// LLVM's `rewrite-statepoints-for-gc` pass uses this fixed ID by default. `runtime-native` uses it
/// as a convention to cheaply identify statepoint-shaped stackmap records when running in
/// verification mode.
const DEFAULT_STATEPOINT_ID: u64 = 0xABCDEF00;

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
///
/// ## LLVM 18 opaque pointers: indirect callees need `elementtype(<fn-ty>)`
///
/// LLVM 18 runs in opaque-pointer mode by default, so a callee value of type
/// `ptr` does not carry its function signature. LLVM therefore requires the
/// callee operand passed to `gc.statepoint` to be annotated with
/// `elementtype(<callee function type>)`.
///
/// For direct calls this signature can be recovered from a `@function` global
/// via `LLVMGlobalGetValueType`.
///
/// For *indirect* calls (a runtime function pointer value like `%fp`), callers
/// must provide the callee function type explicitly via
/// [`StatepointEmitter::emit_statepoint_call_indirect`].
///
/// ## Important: GC pointer call arguments are roots
/// LLVM does **not** automatically treat `ptr addrspace(1)` call arguments to a statepointed call
/// as GC roots for stack map emission. Any GC pointer passed as a call argument must also appear in
/// the `"gc-live"` operand bundle (and have corresponding `gc.relocate` users) or it may be absent
/// from the stack map record.
///
/// This emitter therefore automatically extends `"gc-live"` with any `ptr addrspace(1)` values
/// found in `call_args`, so callers can't accidentally omit them.
pub struct StatepointEmitter {
  ctx: LLVMContextRef,
  module: LLVMModuleRef,
  statepoint_intrinsic_id: u32,
  gc_result_intrinsic_id: u32,
  gc_relocate_intrinsic_id: u32,
  elementtype_attr_kind: u32,
  i32_ty: LLVMTypeRef,
  i64_ty: LLVMTypeRef,
  statepoint_decls: HashMap<usize, IntrinsicDecl>,
  gc_result_decls: HashMap<usize, IntrinsicDecl>,
  gc_relocate_decls: HashMap<usize, IntrinsicDecl>,
}

#[derive(Copy, Clone)]
struct IntrinsicDecl {
  func: LLVMValueRef,
  ty: LLVMTypeRef,
}

impl StatepointEmitter {
  pub unsafe fn new(ctx: LLVMContextRef, module: LLVMModuleRef, gc_ptr_ty: LLVMTypeRef) -> Self {
    let i32_ty = LLVMIntTypeInContext(ctx, 32);
    let i64_ty = LLVMIntTypeInContext(ctx, 64);

    let statepoint_name = b"llvm.experimental.gc.statepoint";
    let gc_result_name = b"llvm.experimental.gc.result";
    let gc_relocate_name = b"llvm.experimental.gc.relocate";

    let statepoint_intrinsic_id =
      LLVMLookupIntrinsicID(statepoint_name.as_ptr().cast(), statepoint_name.len());
    let gc_result_intrinsic_id =
      LLVMLookupIntrinsicID(gc_result_name.as_ptr().cast(), gc_result_name.len());
    let gc_relocate_intrinsic_id =
      LLVMLookupIntrinsicID(gc_relocate_name.as_ptr().cast(), gc_relocate_name.len());

    assert!(statepoint_intrinsic_id != 0, "missing LLVM intrinsic: gc.statepoint");
    assert!(gc_result_intrinsic_id != 0, "missing LLVM intrinsic: gc.result");
    assert!(gc_relocate_intrinsic_id != 0, "missing LLVM intrinsic: gc.relocate");

    let elementtype_attr_kind =
      LLVMGetEnumAttributeKindForName("elementtype\0".as_ptr().cast(), "elementtype".len());
    assert!(
      elementtype_attr_kind != 0,
      "missing LLVM attribute kind: elementtype"
    );

    let mut out = Self {
      ctx,
      module,
      statepoint_intrinsic_id,
      gc_result_intrinsic_id,
      gc_relocate_intrinsic_id,
      elementtype_attr_kind,
      i32_ty,
      i64_ty,
      statepoint_decls: HashMap::new(),
      gc_result_decls: HashMap::new(),
      gc_relocate_decls: HashMap::new(),
    };

    // Pre-warm the `gc.relocate` cache for the project's canonical GC pointer type so most call
    // sites avoid a lookup.
    out.get_gc_relocate_decl(gc_ptr_ty);
    out
  }

  unsafe fn get_statepoint_decl(&mut self, callee_ptr_ty: LLVMTypeRef) -> IntrinsicDecl {
    let key = callee_ptr_ty as usize;
    if let Some(&decl) = self.statepoint_decls.get(&key) {
      return decl;
    }

    let mut overloads = [callee_ptr_ty];
    let func = LLVMGetIntrinsicDeclaration(
      self.module,
      self.statepoint_intrinsic_id,
      overloads.as_mut_ptr(),
      overloads.len(),
    );
    let ty = LLVMGlobalGetValueType(func);
    let decl = IntrinsicDecl { func, ty };
    self.statepoint_decls.insert(key, decl);
    decl
  }

  unsafe fn get_gc_result_decl(&mut self, ret_ty: LLVMTypeRef) -> IntrinsicDecl {
    let key = ret_ty as usize;
    if let Some(&decl) = self.gc_result_decls.get(&key) {
      return decl;
    }

    let mut overloads = [ret_ty];
    let func = LLVMGetIntrinsicDeclaration(
      self.module,
      self.gc_result_intrinsic_id,
      overloads.as_mut_ptr(),
      overloads.len(),
    );
    let ty = LLVMGlobalGetValueType(func);
    let decl = IntrinsicDecl { func, ty };
    self.gc_result_decls.insert(key, decl);
    decl
  }

  unsafe fn get_gc_relocate_decl(&mut self, ptr_ty: LLVMTypeRef) -> IntrinsicDecl {
    let key = ptr_ty as usize;
    if let Some(&decl) = self.gc_relocate_decls.get(&key) {
      return decl;
    }

    let mut overloads = [ptr_ty];
    let func = LLVMGetIntrinsicDeclaration(
      self.module,
      self.gc_relocate_intrinsic_id,
      overloads.as_mut_ptr(),
      overloads.len(),
    );
    let ty = LLVMGlobalGetValueType(func);
    let decl = IntrinsicDecl { func, ty };
    self.gc_relocate_decls.insert(key, decl);
    decl
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
    // Base pointers are identical to derived pointers for non-interior roots.
    let base_indices: Vec<u32> = (0..gc_live.len() as u32).collect();
    self.emit_statepoint_call_with_base_indices(builder, callee, call_args, gc_live, &base_indices)
  }

  /// Like [`emit_statepoint_call`], but allows specifying a base-pointer index for each
  /// relocated value.
  ///
  /// This is required for interior pointers (derived pointers) where `base_idx != derived_idx`.
  /// Indices are 0-based into the `"gc-live"` operand bundle list.
  pub unsafe fn emit_statepoint_call_with_base_indices(
    &mut self,
    builder: LLVMBuilderRef,
    callee: LLVMValueRef,
    call_args: &[LLVMValueRef],
    gc_live: &[LLVMValueRef],
    base_indices: &[u32],
  ) -> StatepointCall {
    debug_assert_eq!(
      gc_live.len(),
      base_indices.len(),
      "base_indices must match gc_live length"
    );
    let callee_fn_ty = LLVMGlobalGetValueType(callee);
    self.emit_statepoint_call_indirect(builder, callee, callee_fn_ty, call_args, gc_live, base_indices)
  }

  /// Emit `gc.statepoint` for an *indirect* callee (`ptr %fp`).
  ///
  /// `callee_fn_ty` must be the callee's *function type* (not a pointer type).
  pub unsafe fn emit_statepoint_call_indirect(
    &mut self,
    builder: LLVMBuilderRef,
    callee_ptr: LLVMValueRef,
    callee_fn_ty: LLVMTypeRef,
    call_args: &[LLVMValueRef],
    gc_live: &[LLVMValueRef],
    base_indices: &[u32],
  ) -> StatepointCall {
    debug_assert_eq!(
      gc_live.len(),
      base_indices.len(),
      "base_indices must match gc_live length"
    );
    debug_assert_eq!(
      LLVMGetTypeKind(callee_fn_ty),
      LLVMTypeKind::LLVMFunctionTypeKind,
      "callee_fn_ty must be a function type"
    );
    let callee_ret_ty = LLVMGetReturnType(callee_fn_ty);
    let callee_ret_kind = LLVMGetTypeKind(callee_ret_ty);

    // `gc.statepoint` argument layout (LLVM 18 opaque pointers):
    //   (id, patch_bytes, callee, num_call_args, flags,
    //    call_args...,
    //    num_transition_args, transition_args...,
    //    num_deopt_args, deopt_args...)
    //
    // In the PoC we emit no transition/deopt args and carry live pointers via
    // the `"gc-live"` operand bundle.
    let mut sp_args = Vec::with_capacity(5 + call_args.len() + 2);
    sp_args.push(LLVMConstInt(self.i64_ty, DEFAULT_STATEPOINT_ID, 0));
    // patch_bytes = 0 (normal call; patchable callsites reserve space with patch_bytes>0).
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0));
    sp_args.push(callee_ptr);
    sp_args.push(LLVMConstInt(self.i32_ty, call_args.len() as u64, 0));
    // flags (LLVM 18 verifier currently accepts only 0..=3; project default is 0).
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0));
    sp_args.extend_from_slice(call_args);
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0)); // num_transition_args
    sp_args.push(LLVMConstInt(self.i32_ty, 0, 0)); // num_deopt_args

    // Attach `elementtype(...)` to the callee operand (required under opaque pointers).
    let elementtype_attr = LLVMCreateTypeAttribute(self.ctx, self.elementtype_attr_kind, callee_fn_ty);

    let statepoint = {
      let callee_ptr_ty = LLVMTypeOf(callee_ptr);
      self.get_statepoint_decl(callee_ptr_ty)
    };
    // Build a `gc-live` list that includes explicit roots plus any GC pointer call arguments.
    //
    // Deterministic ordering:
    //   1) all explicit `gc_live` values (caller-provided order),
    //   2) then unique `ptr addrspace(1)` call args in call-arg order.
    let mut gc_live_values: Vec<LLVMValueRef> = gc_live.to_vec();
    let mut gc_live_index: HashMap<LLVMValueRef, u32> = HashMap::with_capacity(gc_live_values.len());
    for (idx, &v) in gc_live_values.iter().enumerate() {
      gc_live_index.insert(v, idx as u32);
    }

    let mut extra_base_indices: Vec<u32> = Vec::new();
    for &arg in call_args {
      let ty = LLVMTypeOf(arg);
      if LLVMGetTypeKind(ty) == LLVMTypeKind::LLVMPointerTypeKind
        && LLVMGetPointerAddressSpace(ty) == GC_ADDR_SPACE
      {
        if gc_live_index.contains_key(&arg) {
          continue;
        }
        let idx = gc_live_values.len() as u32;
        gc_live_values.push(arg);
        gc_live_index.insert(arg, idx);
        // Base == derived for call arguments (we don't track interior ptrs here).
        extra_base_indices.push(idx);
      }
    }

    let mut bundles: Vec<LLVMOperandBundleRef> = Vec::new();
    if !gc_live_values.is_empty() {
      // `gc-live` operand bundle.
      let name = CString::new("gc-live").unwrap();
      let bundle = LLVMCreateOperandBundle(
        name.as_ptr(),
        name.as_bytes().len(),
        gc_live_values.as_ptr().cast_mut(),
        gc_live_values.len() as u32,
      );
      bundles.push(bundle);
    }

    let bundles_ptr = if bundles.is_empty() {
      std::ptr::null_mut()
    } else {
      bundles.as_mut_ptr()
    };
    let token = LLVMBuildCallWithOperandBundles(
      builder,
      statepoint.ty,
      statepoint.func,
      sp_args.as_mut_ptr(),
      sp_args.len() as u32,
      bundles_ptr,
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
      let gc_result = self.get_gc_result_decl(callee_ret_ty);

      Some(LLVMBuildCall2(
        builder,
        gc_result.ty,
        gc_result.func,
        [token].as_mut_ptr(),
        1,
        b"gc_result\0".as_ptr().cast(),
      ))
    };

    let mut relocated_all = Vec::with_capacity(gc_live_values.len());
    let mut combined_base_indices: Vec<u32> = Vec::with_capacity(gc_live_values.len());
    combined_base_indices.extend_from_slice(base_indices);
    combined_base_indices.extend_from_slice(&extra_base_indices);

    for (derived_idx, &base_idx) in combined_base_indices.iter().enumerate() {
      debug_assert!(
        (base_idx as usize) < gc_live_values.len(),
        "base index {base_idx} out of bounds for gc_live length {}",
        gc_live_values.len()
      );
      let base_idx_const = LLVMConstInt(self.i32_ty, base_idx as u64, 0);
      let derived_idx_const = LLVMConstInt(self.i32_ty, derived_idx as u64, 0);
      let derived_ptr_ty = LLVMTypeOf(gc_live_values[derived_idx]);
      let gc_relocate = self.get_gc_relocate_decl(derived_ptr_ty);
      let relocate = LLVMBuildCall2(
        builder,
        gc_relocate.ty,
        gc_relocate.func,
        [token, base_idx_const, derived_idx_const].as_mut_ptr(),
        3,
        CString::new(format!("gc_relocate{derived_idx}"))
          .unwrap()
          .as_ptr(),
      );
      llvm_sys::core::LLVMSetInstructionCallConv(relocate, LLVMCallConv::LLVMColdCallConv as u32);
      relocated_all.push(relocate);
    }

    StatepointCall {
      token,
      result,
      relocated: relocated_all.into_iter().take(gc_live.len()).collect(),
    }
  }

  /// Convenience wrapper for the common case where the callee is `void`.
  ///
  /// Returns the relocated GC pointers (one per `gc_live` input) and does not
  /// attempt to emit a `gc.result`.
  pub unsafe fn emit_statepoint_call_void(
    &mut self,
    builder: LLVMBuilderRef,
    callee: LLVMValueRef,
    call_args: &[LLVMValueRef],
    gc_live: &[LLVMValueRef],
  ) -> Vec<LLVMValueRef> {
    let StatepointCall {
      result, relocated, ..
    } = self.emit_statepoint_call(builder, callee, call_args, gc_live);
    debug_assert!(
      result.is_none(),
      "emit_statepoint_call_void used with non-void callee"
    );
    relocated
  }
}
