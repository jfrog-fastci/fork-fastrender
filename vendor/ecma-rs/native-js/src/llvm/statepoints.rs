//! Helpers for *manual* construction of LLVM `gc.statepoint` intrinsics.
//!
//! Most of `native-js` relies on LLVM's `rewrite-statepoints-for-gc` pass rather than emitting
//! `@llvm.experimental.gc.statepoint` directly. However, a few situations (tests, bespoke safepoint
//! polls, minimal reproducers) still need to build verifier-correct statepoints by hand.
//!
//! ## LLVM 18 + opaque pointers: `elementtype` is mandatory
//!
//! In LLVM 18 with opaque pointers, the callee operand passed to
//! `@llvm.experimental.gc.statepoint.*` **must** carry an `elementtype(<fn-ty>)` attribute.
//!
//! For direct calls where the callee is a [`FunctionValue`], the function signature is available
//! and we can attach it automatically.
//!
//! For indirect calls where the callee is only a runtime [`PointerValue`] (`ptr %fp`), LLVM cannot
//! recover the pointee function type from the pointer value, so callers must provide it explicitly
//! via [`build_statepoint_call_indirect`]’s `callee_sig` parameter. We then attach
//! `elementtype(callee_sig)` to the **callee argument** of the statepoint call.
//!
//! This ensures LLVM prints IR like:
//!
//! ```llvm
//! ptr elementtype(void ()) %fp
//! ```
//!
//! and avoids verifier errors such as:
//!
//! ```text
//! gc.statepoint callee argument must have elementtype attribute
//! ```
//!
//! See `vendor/ecma-rs/docs/llvm_statepoints_llvm18.md` for a full description of the required IR
//! shape.
//!
//! ## Important: GC pointer call arguments are roots
//! LLVM does **not** implicitly treat `ptr addrspace(1)` call arguments to a statepoint as GC
//! roots for stack map emission. Any GC pointer passed as a call argument must also appear in the
//! `"gc-live"` operand bundle, or it may be missing from the stack map record.
//!
//! This module therefore automatically extends `"gc-live"` with any `ptr addrspace(1)` call
//! arguments so callers can't accidentally omit them.

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{AsTypeRef, FunctionType};
use inkwell::values::{AsValueRef, BasicMetadataValueEnum, FunctionValue, PointerValue};
use inkwell::AddressSpace;
use llvm_sys::core::{
  LLVMAddAttributeAtIndex, LLVMAddCallSiteAttribute, LLVMAddFunction,
  LLVMBuildCallWithOperandBundles, LLVMCreateEnumAttribute, LLVMCreateOperandBundle,
  LLVMCreateTypeAttribute, LLVMDisposeOperandBundle, LLVMFunctionType,
  LLVMGetEnumAttributeKindForName, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMGetPointerAddressSpace, LLVMGetTypeKind, LLVMTokenTypeInContext, LLVMTypeOf,
};
use llvm_sys::prelude::{LLVMTypeRef, LLVMValueRef};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::raw::c_uint;

use crate::llvm::gc::GC_ADDR_SPACE;

/// Arguments that control how the `gc.statepoint` intrinsic is emitted.
#[derive(Debug, Clone, Copy)]
pub struct StatepointConfig {
  /// The statepoint ID (becomes the StackMap record's patchpoint ID).
  pub id: u64,
  /// Reserved for patchable callsites.
  ///
  /// - `0`: LLVM emits a normal call.
  /// - `>0`: LLVM reserves a patchable region at the callsite (x86_64: a NOP sled) and the
  ///   stackmap record key (`instruction offset`) points to the end of that reserved region.
  ///
  /// See `docs/llvm_statepoint_stackmap_abi.md` for the project-level ABI assumptions.
  pub num_patch_bytes: u32,
  /// Statepoint flags.
  ///
  /// On LLVM 18, the IR verifier only accepts values in the range `0..=3` (two-bit mask).
  /// This project currently uses `0`.
  pub flags: u32,
}

impl Default for StatepointConfig {
  fn default() -> Self {
    Self {
      id: 0,
      num_patch_bytes: 0,
      flags: 0,
    }
  }
}

const ELEMENTTYPE_ATTR: &str = "elementtype";
const IMMARG_ATTR: &str = "immarg";
const GC_LIVE_BUNDLE: &str = "gc-live";

struct StatepointDecl {
  func: LLVMValueRef,
  func_ty: LLVMTypeRef,
}

fn statepoint_p0_decl<'ctx>(ctx: &'ctx Context, module: &Module<'ctx>) -> StatepointDecl {
  // Canonical declaration (LLVM 18):
  //   declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
  let module_ref = module.as_mut_ptr();
  let llvm_ctx = unsafe { LLVMGetModuleContext(module_ref) };

  let token_ty = unsafe { LLVMTokenTypeInContext(llvm_ctx) };
  let i64_ty = ctx.i64_type();
  let i32_ty = ctx.i32_type();
  let ptr_ty = ctx.ptr_type(AddressSpace::default());

  let mut params = [
    i64_ty.as_type_ref(),
    i32_ty.as_type_ref(),
    ptr_ty.as_type_ref(),
    i32_ty.as_type_ref(),
    i32_ty.as_type_ref(),
  ];
  let fn_ty = unsafe { LLVMFunctionType(token_ty, params.as_mut_ptr(), params.len() as c_uint, 1) };

  let name = CString::new("llvm.experimental.gc.statepoint.p0")
    .expect("statepoint intrinsic name contains NUL");
  let mut f = unsafe { LLVMGetNamedFunction(module_ref, name.as_ptr()) };
  if f.is_null() {
    f = unsafe { LLVMAddFunction(module_ref, name.as_ptr(), fn_ty) };

    // Add the `immarg` attribute on the non-variadic immediate parameters so our IR matches the
    // intrinsic declaration and the verifier can enforce immediates.
    let immarg_c = CString::new(IMMARG_ATTR).expect("immarg contains NUL");
    let immarg_kind =
      unsafe { LLVMGetEnumAttributeKindForName(immarg_c.as_ptr(), immarg_c.as_bytes().len()) };
    if immarg_kind != 0 {
      let immarg_attr = unsafe { LLVMCreateEnumAttribute(llvm_ctx, immarg_kind, 0) };
      unsafe {
        // Param indices are 1-based (0 is return, and LLVMAttributeFunctionIndex is for fn attrs).
        for idx in [1u32, 2, 4, 5] {
          LLVMAddAttributeAtIndex(f, idx, immarg_attr);
        }
      }
    }
  }

  StatepointDecl {
    func: f,
    func_ty: fn_ty,
  }
}

fn attach_elementtype_to_statepoint_callee<'ctx>(
  module: &Module<'ctx>,
  call: LLVMValueRef,
  callee_sig: FunctionType<'ctx>,
) {
  let llvm_ctx = unsafe { LLVMGetModuleContext(module.as_mut_ptr()) };
  let elementtype_c = CString::new(ELEMENTTYPE_ATTR).expect("elementtype contains NUL");
  let kind = unsafe {
    LLVMGetEnumAttributeKindForName(elementtype_c.as_ptr(), elementtype_c.as_bytes().len())
  };
  assert!(
    kind != 0,
    "LLVM did not recognize `{ELEMENTTYPE_ATTR}` attribute kind"
  );

  let attr = unsafe { LLVMCreateTypeAttribute(llvm_ctx, kind, callee_sig.as_type_ref()) };
  unsafe {
    // The statepoint callee is the 3rd intrinsic parameter:
    //   (i64 id, i32 num_patch_bytes, ptr callee, ...)
    LLVMAddCallSiteAttribute(call, 3, attr);
  }
}

/// Build a verifier-correct `gc.statepoint` call to a *direct* [`FunctionValue`] callee.
///
/// This is a convenience wrapper around [`build_statepoint_call_indirect`].
pub fn build_statepoint_call_direct<'ctx>(
  ctx: &'ctx Context,
  module: &Module<'ctx>,
  builder: &Builder<'ctx>,
  config: StatepointConfig,
  callee: FunctionValue<'ctx>,
  call_args: &[BasicMetadataValueEnum<'ctx>],
  gc_live: &[PointerValue<'ctx>],
  name: &str,
) -> LLVMValueRef {
  build_statepoint_call_indirect(
    ctx,
    module,
    builder,
    config,
    callee.as_global_value().as_pointer_value(),
    callee.get_type(),
    call_args,
    gc_live,
    name,
  )
}

/// Build a verifier-correct `gc.statepoint` call to an *indirect* callee (`ptr %fp`).
///
/// `callee_sig` is required to satisfy LLVM 18's opaque-pointer verifier requirement that the
/// statepoint callee argument carries `elementtype(<fn-ty>)`.
pub fn build_statepoint_call_indirect<'ctx>(
  ctx: &'ctx Context,
  module: &Module<'ctx>,
  builder: &Builder<'ctx>,
  config: StatepointConfig,
  callee_ptr: PointerValue<'ctx>,
  callee_sig: FunctionType<'ctx>,
  call_args: &[BasicMetadataValueEnum<'ctx>],
  gc_live: &[PointerValue<'ctx>],
  name: &str,
) -> LLVMValueRef {
  assert!(
    config.flags <= 3,
    "LLVM 18 accepts only gc.statepoint flags in range 0..=3 (two-bit mask); got {}",
    config.flags
  );

  let statepoint = statepoint_p0_decl(ctx, module);

  let i64_ty = ctx.i64_type();
  let i32_ty = ctx.i32_type();
  let id = i64_ty.const_int(config.id, false);
  let patch = i32_ty.const_int(config.num_patch_bytes as u64, false);
  let num_call_args = i32_ty.const_int(call_args.len() as u64, false);
  let flags = i32_ty.const_int(config.flags as u64, false);
  let zero_i32 = i32_ty.const_zero();

  let mut args = Vec::with_capacity(5 + call_args.len() + 2);
  args.push(id.as_value_ref());
  args.push(patch.as_value_ref());
  args.push(callee_ptr.as_value_ref());
  args.push(num_call_args.as_value_ref());
  args.push(flags.as_value_ref());
  for arg in call_args {
    args.push(arg.as_value_ref());
  }
  // LLVM 18 requires these final two constant i32 operands (both must be 0).
  args.push(zero_i32.as_value_ref()); // numTransitionArgs
  args.push(zero_i32.as_value_ref()); // numDeoptArgs

  let mut bundles = Vec::new();
  let mut live_values: Vec<LLVMValueRef> = gc_live.iter().map(|v| v.as_value_ref()).collect();
  let mut live_set: HashSet<LLVMValueRef> = live_values.iter().copied().collect();

  // LLVM does not implicitly treat GC pointer call arguments as roots for stack map emission. Any
  // `ptr addrspace(1)` passed as a call argument must also be listed in `"gc-live"`.
  //
  // Deterministic ordering:
  //   1) caller-provided `gc_live` values (in order),
  //   2) then unique GC pointer call args (in call-arg order).
  for arg in call_args {
    let v = arg.as_value_ref();
    unsafe {
      let ty = LLVMTypeOf(v);
      if LLVMGetTypeKind(ty) == llvm_sys::LLVMTypeKind::LLVMPointerTypeKind
        && LLVMGetPointerAddressSpace(ty) == GC_ADDR_SPACE
      {
        if live_set.insert(v) {
          live_values.push(v);
        }
      }
    }
  }
  let gc_live_name = CString::new(GC_LIVE_BUNDLE).expect("gc-live contains NUL");
  let live_ptr = if live_values.is_empty() {
    std::ptr::null_mut()
  } else {
    live_values.as_mut_ptr()
  };
  let bundle = unsafe {
    LLVMCreateOperandBundle(
      gc_live_name.as_ptr(),
      gc_live_name.as_bytes().len(),
      live_ptr,
      live_values.len() as c_uint,
    )
  };
  bundles.push(bundle);

  let name = CString::new(name).expect("statepoint name contains NUL");
  let call = unsafe {
    LLVMBuildCallWithOperandBundles(
      builder.as_mut_ptr(),
      statepoint.func_ty,
      statepoint.func,
      args.as_mut_ptr(),
      args.len() as c_uint,
      bundles.as_mut_ptr(),
      bundles.len() as c_uint,
      name.as_ptr(),
    )
  };

  // The builder copies operand bundles, so we can dispose of the temporary defs.
  unsafe {
    for b in bundles {
      LLVMDisposeOperandBundle(b);
    }
  }

  attach_elementtype_to_statepoint_callee(module, call, callee_sig);
  call
}
