//! LLVM GC statepoint emission helpers.
//!
//! ## Why statepoints?
//! LLVM's normal call lowering does not automatically produce GC-visible metadata for where
//! GC-managed pointers live on the stack/registers at each safepoint. For a precise (and
//! eventually moving) GC we need *stack maps* that tell the runtime exactly which values are live
//! at a call/safepoint.
//!
//! LLVM's "statepoint" GC strategy solves this by rewriting (or directly emitting) calls as
//! `@llvm.experimental.gc.statepoint` intrinsics. LLVM then emits precise stack maps for those
//! call sites.
//!
//! ## `gc.result` / `gc.relocate`
//! A statepoint call returns a `token` rather than the callee's return value. If the wrapped call
//! returns non-void, the actual value is recovered via `@llvm.experimental.gc.result`.
//!
//! Any GC-managed pointer that is live across the safepoint must be "reloaded" after the
//! statepoint, because a moving GC may relocate the referenced object. This is done via
//! `@llvm.experimental.gc.relocate`. Even with a non-moving GC today, emitting `gc.relocate` makes
//! the IR future-proof: later we can switch to a moving collector without changing codegen.
//!
//! ## Derived / interior pointers
//! `LiveGcPtr` models LLVM's base+derived relocation scheme.
//!
//! - For normal GC references, use [`LiveGcPtr::new`] (base == derived).
//! - For interior pointers (e.g. `getelementptr` results that remain live across a safepoint), use
//!   [`LiveGcPtr::new_with_base`].
//!
//! If you can cheaply recompute the interior pointer after the safepoint, it is usually better to
//! keep only the base pointer live and redo the `gep` from the relocated base.
//!
//! ## Important: GC pointer call arguments are roots
//! LLVM does **not** automatically treat `ptr addrspace(1)` call arguments to a statepoint as GC
//! roots when emitting stack maps. Any GC pointer passed as a call argument must also appear in the
//! `"gc-live"` operand bundle or the pointer will be missing from the stack map record.
//!
//! This module therefore automatically extends `"gc-live"` with any `ptr addrspace(1)` call
//! arguments so callers can't accidentally omit them.

use std::collections::HashMap;
use std::ffi::CString;
use std::marker::PhantomData;

use inkwell::builder::Builder;
use inkwell::module::Module;
use inkwell::types::AsTypeRef;
use inkwell::types::{BasicTypeEnum, FunctionType, PointerType};
use inkwell::values::AsValueRef;
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, PointerValue};
use llvm_sys::core::{
  LLVMAddCallSiteAttribute, LLVMBuildAlloca, LLVMBuildCall2, LLVMBuildCallWithOperandBundles,
  LLVMBuildStore, LLVMConstInt, LLVMCreateBuilderInContext, LLVMCreateOperandBundle,
  LLVMCreateTypeAttribute, LLVMDisposeBuilder, LLVMDisposeOperandBundle, LLVMGetBasicBlockParent,
  LLVMGetEnumAttributeKindForName, LLVMGetFirstBasicBlock, LLVMGetFirstInstruction,
  LLVMGetInsertBlock, LLVMGetIntrinsicDeclaration, LLVMGetModuleContext,
  LLVMGetPointerAddressSpace, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMInt32TypeInContext,
  LLVMInt64TypeInContext, LLVMLookupIntrinsicID, LLVMPositionBuilderAtEnd,
  LLVMPositionBuilderBefore, LLVMSetInstructionCallConv, LLVMSetVolatile, LLVMTypeOf,
};
use llvm_sys::prelude::{
  LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMTypeRef, LLVMValueRef,
};
use llvm_sys::LLVMCallConv;
use llvm_sys::LLVMTypeKind;

use crate::llvm::gc::GC_ADDR_SPACE;
use super::LLVM_STATEPOINT_PATCHPOINT_ID;

/// A (base, derived) GC pointer pair live across a safepoint.
///
/// For non-interior pointers, `base == derived`.
#[derive(Copy, Clone)]
pub struct LiveGcPtr<'ctx> {
  pub base: PointerValue<'ctx>,
  pub derived: PointerValue<'ctx>,
}

impl<'ctx> LiveGcPtr<'ctx> {
  #[inline]
  pub fn new(ptr: PointerValue<'ctx>) -> Self {
    Self {
      base: ptr,
      derived: ptr,
    }
  }

  #[inline]
  pub fn new_with_base(base: PointerValue<'ctx>, derived: PointerValue<'ctx>) -> Self {
    Self { base, derived }
  }
}

/// Describes the target being wrapped by a statepoint.
///
/// We need both the callee *value* (a `ptr`) and its LLVM `FunctionType` to emit the
/// `elementtype(<fn ty>)` parameter attribute required by the statepoint intrinsic.
#[derive(Copy, Clone)]
pub struct StatepointCallee<'ctx> {
  pub ptr: PointerValue<'ctx>,
  pub ty: FunctionType<'ctx>,
}

impl<'ctx> StatepointCallee<'ctx> {
  #[inline]
  pub fn new(ptr: PointerValue<'ctx>, ty: FunctionType<'ctx>) -> Self {
    Self { ptr, ty }
  }
}

/// Cached declarations for LLVM's experimental statepoint intrinsics.
///
/// This struct is intentionally small: codegen should not need to know the exact intrinsic
/// signatures or how to construct operand bundles.
pub struct StatepointIntrinsics<'ctx> {
  module: LLVMModuleRef,
  // NOTE: Keep the context around for attribute creation.
  context: LLVMContextRef,
  alloca_builder: LLVMBuilderRef,

  statepoint_intrinsic_id: u32,
  gc_result_intrinsic_id: u32,
  gc_relocate_intrinsic_id: u32,
  elementtype_attr_kind: u32,

  // Cache declarations by overloaded type key.
  statepoint_decls: HashMap<usize, LLVMValueRef>,
  gc_result_decls: HashMap<usize, LLVMValueRef>,
  gc_relocate_decls: HashMap<usize, LLVMValueRef>,
  // Per-function sinks used to keep `gc.relocate` results for auto-rooted call arguments alive.
  call_arg_sinks: HashMap<(usize, usize), LLVMValueRef>,

  _marker: PhantomData<&'ctx ()>,
}

impl<'ctx> StatepointIntrinsics<'ctx> {
  pub fn new(module: &Module<'ctx>) -> Self {
    // `Module::as_mut_ptr` is stable in Inkwell and gives us the raw `LLVMModuleRef`.
    let module_ref = module.as_mut_ptr();
    let context_ref = unsafe { LLVMGetModuleContext(module_ref) };

    unsafe {
      let statepoint_name = b"llvm.experimental.gc.statepoint";
      let gc_result_name = b"llvm.experimental.gc.result";
      let gc_relocate_name = b"llvm.experimental.gc.relocate";

      let statepoint_intrinsic_id =
        LLVMLookupIntrinsicID(statepoint_name.as_ptr().cast(), statepoint_name.len());
      let gc_result_intrinsic_id =
        LLVMLookupIntrinsicID(gc_result_name.as_ptr().cast(), gc_result_name.len());
      let gc_relocate_intrinsic_id =
        LLVMLookupIntrinsicID(gc_relocate_name.as_ptr().cast(), gc_relocate_name.len());

      let elementtype_attr_kind =
        LLVMGetEnumAttributeKindForName(b"elementtype".as_ptr().cast(), "elementtype".len());

      assert!(
        statepoint_intrinsic_id != 0,
        "missing LLVM intrinsic: gc.statepoint"
      );
      assert!(
        gc_result_intrinsic_id != 0,
        "missing LLVM intrinsic: gc.result"
      );
      assert!(
        gc_relocate_intrinsic_id != 0,
        "missing LLVM intrinsic: gc.relocate"
      );
      assert!(
        elementtype_attr_kind != 0,
        "missing LLVM attribute kind: elementtype"
      );

      Self {
        module: module_ref,
        context: context_ref,
        alloca_builder: LLVMCreateBuilderInContext(context_ref),
        statepoint_intrinsic_id,
        gc_result_intrinsic_id,
        gc_relocate_intrinsic_id,
        elementtype_attr_kind,
        statepoint_decls: HashMap::new(),
        gc_result_decls: HashMap::new(),
        gc_relocate_decls: HashMap::new(),
        call_arg_sinks: HashMap::new(),
        _marker: PhantomData,
      }
    }
  }

  unsafe fn get_call_arg_sink_alloca(
    &mut self,
    builder: LLVMBuilderRef,
    ptr_ty: LLVMTypeRef,
  ) -> LLVMValueRef {
    let bb = LLVMGetInsertBlock(builder);
    debug_assert!(!bb.is_null(), "builder has no insert block");
    let func = LLVMGetBasicBlockParent(bb);
    debug_assert!(!func.is_null(), "insert block has no parent function");

    let key = (func as usize, ptr_ty as usize);
    if let Some(&alloca) = self.call_arg_sinks.get(&key) {
      return alloca;
    }

    let entry: LLVMBasicBlockRef = LLVMGetFirstBasicBlock(func);
    debug_assert!(!entry.is_null(), "function has no entry block");
    let first = LLVMGetFirstInstruction(entry);
    if first.is_null() {
      LLVMPositionBuilderAtEnd(self.alloca_builder, entry);
    } else {
      LLVMPositionBuilderBefore(self.alloca_builder, first);
    }

    let alloca = LLVMBuildAlloca(
      self.alloca_builder,
      ptr_ty,
      b"gc_call_arg_sink\0".as_ptr().cast(),
    );
    self.call_arg_sinks.insert(key, alloca);
    alloca
  }

  fn get_statepoint_decl(&mut self, callee_ptr_ty: PointerType<'ctx>) -> LLVMValueRef {
    let key = callee_ptr_ty.as_type_ref() as usize;
    if let Some(&f) = self.statepoint_decls.get(&key) {
      return f;
    }

    let overloaded = [callee_ptr_ty.as_type_ref()];
    let decl = unsafe {
      LLVMGetIntrinsicDeclaration(
        self.module,
        self.statepoint_intrinsic_id,
        overloaded.as_ptr() as *mut LLVMTypeRef,
        overloaded.len(),
      )
    };

    self.statepoint_decls.insert(key, decl);
    decl
  }

  fn get_gc_result_decl(&mut self, ret_ty: BasicTypeEnum<'ctx>) -> LLVMValueRef {
    let key = ret_ty.as_type_ref() as usize;
    if let Some(&f) = self.gc_result_decls.get(&key) {
      return f;
    }

    let overloaded = [ret_ty.as_type_ref()];
    let decl = unsafe {
      LLVMGetIntrinsicDeclaration(
        self.module,
        self.gc_result_intrinsic_id,
        overloaded.as_ptr() as *mut LLVMTypeRef,
        overloaded.len(),
      )
    };

    self.gc_result_decls.insert(key, decl);
    decl
  }

  fn get_gc_relocate_decl(&mut self, ptr_ty: PointerType<'ctx>) -> LLVMValueRef {
    let key = ptr_ty.as_type_ref() as usize;
    if let Some(&f) = self.gc_relocate_decls.get(&key) {
      return f;
    }

    let overloaded = [ptr_ty.as_type_ref()];
    let decl = unsafe {
      LLVMGetIntrinsicDeclaration(
        self.module,
        self.gc_relocate_intrinsic_id,
        overloaded.as_ptr() as *mut LLVMTypeRef,
        overloaded.len(),
      )
    };

    self.gc_relocate_decls.insert(key, decl);
    decl
  }

  /// Emit a statepointed call.
  ///
  /// - `call_args` are the arguments for the *callee*.
  /// - `live_gc_ptrs` are GC-managed pointers that must be considered live across the call.
  ///   Additionally, any `ptr addrspace(1)` values found in `call_args` are treated as live GC
  ///   pointers as well.
  /// - If `ret_ty` is `Some`, the returned value is produced by `gc.result`.
  /// - Relocated pointers are always produced via `gc.relocate`, even for a non-moving GC.
  pub fn emit_statepoint_call(
    &mut self,
    builder: &Builder<'ctx>,
    callee: StatepointCallee<'ctx>,
    call_args: &[BasicMetadataValueEnum<'ctx>],
    live_gc_ptrs: &[LiveGcPtr<'ctx>],
    ret_ty: Option<BasicTypeEnum<'ctx>>,
  ) -> (Option<BasicValueEnum<'ctx>>, Vec<PointerValue<'ctx>>) {
    // Protect against catastrophic miscompiles: only `ptr addrspace(1)` values are allowed to
    // participate in relocation. Raw/external pointers (malloc/mmap backing stores, iovecs, etc.)
    // must remain `ptr` (addrspace(0)) and must never be fed into `gc.relocate`.
    for live in live_gc_ptrs {
      unsafe {
        let base_ty = LLVMTypeOf(live.base.as_value_ref());
        let derived_ty = LLVMTypeOf(live.derived.as_value_ref());
        assert!(
          LLVMGetTypeKind(base_ty) == LLVMTypeKind::LLVMPointerTypeKind
            && LLVMGetPointerAddressSpace(base_ty) == GC_ADDR_SPACE,
          "LiveGcPtr.base must be a `ptr addrspace({GC_ADDR_SPACE})`"
        );
        assert!(
          LLVMGetTypeKind(derived_ty) == LLVMTypeKind::LLVMPointerTypeKind
            && LLVMGetPointerAddressSpace(derived_ty) == GC_ADDR_SPACE,
          "LiveGcPtr.derived must be a `ptr addrspace({GC_ADDR_SPACE})`"
        );
      }
    }

    let i64_ty = unsafe { LLVMInt64TypeInContext(self.context) };
    let i32_ty = unsafe { LLVMInt32TypeInContext(self.context) };

    // Use the canonical LLVM patchpoint ID for statepoints.
    //
    // `runtime-native` uses this as a convention to cheaply identify statepoint-shaped stackmap
    // records when running in verification mode.
    let statepoint_id = LLVM_STATEPOINT_PATCHPOINT_ID;

    let statepoint_decl = self.get_statepoint_decl(callee.ptr.get_type());
    let statepoint_fn_ty = unsafe { LLVMGlobalGetValueType(statepoint_decl) };

    // Fixed args: (i64 id, i32 patch_bytes, ptr callee, i32 num_call_args, i32 flags, ...)
    let mut args: Vec<LLVMValueRef> = Vec::with_capacity(5 + call_args.len() + 2);
    unsafe {
      args.push(LLVMConstInt(i64_ty, statepoint_id, 0));
      // patch_bytes = 0 (normal call; patchable callsites reserve space with patch_bytes>0).
      args.push(LLVMConstInt(i32_ty, 0, 0));
      args.push(callee.ptr.as_value_ref());
      args.push(LLVMConstInt(i32_ty, call_args.len() as u64, 0));
      // flags (LLVM 18 verifier currently accepts only 0..=3; project default is 0).
      args.push(LLVMConstInt(i32_ty, 0, 0));
    }

    // Call args.
    for arg in call_args {
      args.push(arg.as_value_ref());
    }

    // LLVM's statepoint intrinsic expects two trailing `i32 0` operands (as emitted by
    // `rewrite-statepoints-for-gc` on LLVM 18). These currently represent unimplemented/unused
    // patchpoint fields but are required for verifier correctness.
    unsafe {
      args.push(LLVMConstInt(i32_ty, 0, 0));
      args.push(LLVMConstInt(i32_ty, 0, 0));
    }

    // Build the `gc-live` operand bundle. We include unique pointer values and compute indices
    // for (base, derived) pairs against this list.
    let mut gc_live_values: Vec<LLVMValueRef> = Vec::new();
    let mut gc_live_index: HashMap<LLVMValueRef, u32> = HashMap::new();

    let mut intern_gc_live = |v: LLVMValueRef| -> u32 {
      if let Some(&idx) = gc_live_index.get(&v) {
        return idx;
      }
      let idx = gc_live_values.len() as u32;
      gc_live_values.push(v);
      gc_live_index.insert(v, idx);
      idx
    };

    let mut relocated_derived: HashMap<LLVMValueRef, ()> =
      HashMap::with_capacity(live_gc_ptrs.len());
    // Whether each relocation corresponds to an auto-rooted call argument.
    let mut ptr_indices: Vec<(u32, u32, PointerType<'ctx>, bool)> =
      Vec::with_capacity(live_gc_ptrs.len() + call_args.len());

    for live in live_gc_ptrs {
      let base_idx = intern_gc_live(live.base.as_value_ref());
      let derived_idx = intern_gc_live(live.derived.as_value_ref());
      ptr_indices.push((base_idx, derived_idx, live.derived.get_type(), false));
      relocated_derived.insert(live.derived.as_value_ref(), ());
    }

    for arg in call_args {
      unsafe {
        let v = arg.as_value_ref();
        let ty = LLVMTypeOf(v);
        if LLVMGetTypeKind(ty) == LLVMTypeKind::LLVMPointerTypeKind
          && LLVMGetPointerAddressSpace(ty) == GC_ADDR_SPACE
        {
          let idx = intern_gc_live(v);
          if !relocated_derived.contains_key(&v) {
            ptr_indices.push((idx, idx, PointerValue::new(v).get_type(), true));
            relocated_derived.insert(v, ());
          }
        }
      }
    }

    // `LLVMCreateOperandBundle` requires a nul-terminated C string.
    let bundle_name = CString::new("gc-live").expect("gc-live has no interior nul");
    let statepoint_token = unsafe {
      let bundle = LLVMCreateOperandBundle(
        bundle_name.as_ptr(),
        "gc-live".len(),
        gc_live_values.as_mut_ptr(),
        gc_live_values.len() as u32,
      );
      let bundles = [bundle];

      let inst = LLVMBuildCallWithOperandBundles(
        builder.as_mut_ptr(),
        statepoint_fn_ty,
        statepoint_decl,
        args.as_mut_ptr(),
        args.len() as u32,
        bundles.as_ptr() as *mut _,
        bundles.len() as u32,
        b"statepoint_token\0".as_ptr().cast(),
      );

      LLVMDisposeOperandBundle(bundle);
      inst
    };

    // Add the required `elementtype(<callee fn ty>)` parameter attribute to the callee pointer
    // argument (3rd parameter, 1-based in LLVM's attribute indexing).
    unsafe {
      let attr = LLVMCreateTypeAttribute(
        self.context,
        self.elementtype_attr_kind,
        callee.ty.as_type_ref(),
      );
      LLVMAddCallSiteAttribute(statepoint_token, 3, attr);
    }

    // If non-void, recover the wrapped return value via gc.result.
    let ret_val = ret_ty.map(|ret_ty| {
      let gc_result_decl = self.get_gc_result_decl(ret_ty);
      let gc_result_fn_ty = unsafe { LLVMGlobalGetValueType(gc_result_decl) };
      let mut gc_result_args = [statepoint_token];
      let v = unsafe {
        LLVMBuildCall2(
          builder.as_mut_ptr(),
          gc_result_fn_ty,
          gc_result_decl,
          gc_result_args.as_mut_ptr(),
          gc_result_args.len() as u32,
          b"gc_result\0".as_ptr().cast(),
        )
      };
      unsafe { BasicValueEnum::new(v) }
    });

    let mut relocated = Vec::with_capacity(ptr_indices.len());
    for (base_idx, derived_idx, derived_ty, is_call_arg) in ptr_indices {
      let gc_relocate_decl = self.get_gc_relocate_decl(derived_ty);
      let gc_relocate_fn_ty = unsafe { LLVMGlobalGetValueType(gc_relocate_decl) };

      let mut relocate_args = [
        statepoint_token,
        unsafe { LLVMConstInt(i32_ty, base_idx as u64, 0) },
        unsafe { LLVMConstInt(i32_ty, derived_idx as u64, 0) },
      ];

      let inst = unsafe {
        LLVMBuildCall2(
          builder.as_mut_ptr(),
          gc_relocate_fn_ty,
          gc_relocate_decl,
          relocate_args.as_mut_ptr(),
          relocate_args.len() as u32,
          b"gc_relocate\0".as_ptr().cast(),
        )
      };

      unsafe {
        LLVMSetInstructionCallConv(inst, LLVMCallConv::LLVMColdCallConv as u32);
      }

      // Keep relocates for GC-pointer call arguments alive: even if the relocated value is not used
      // after the statepoint, the `(base, derived)` location pair must stay in the stackmap record
      // so the runtime can update the argument slot/register while executing inside the callee.
      if is_call_arg {
        unsafe {
          let sink = self.get_call_arg_sink_alloca(builder.as_mut_ptr(), derived_ty.as_type_ref());
          let store = LLVMBuildStore(builder.as_mut_ptr(), inst, sink);
          LLVMSetVolatile(store, 1);
        }
      }

      relocated.push(unsafe { PointerValue::new(inst) });
    }

    (ret_val, relocated)
  }
}

impl Drop for StatepointIntrinsics<'_> {
  fn drop(&mut self) {
    unsafe {
      LLVMDisposeBuilder(self.alloca_builder);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::llvm::gc;
  use inkwell::context::Context;
  use inkwell::AddressSpace;

  #[test]
  fn statepoint_gc_live_excludes_raw_ptrs() {
    let ctx = Context::create();
    let module = ctx.create_module("gc_live_smoke");
    let builder = ctx.create_builder();

    let gc_ptr_ty = gc::gc_ptr_type(&ctx);
    let raw_ptr_ty = ctx.ptr_type(AddressSpace::default());

    // Dummy callee taking both a GC pointer and a raw pointer.
    let callee_ty = ctx
      .void_type()
      .fn_type(&[gc_ptr_ty.into(), raw_ptr_ty.into()], false);
    let callee_fn = module.add_function("dummy", callee_ty, None);

    // Caller is a GC-managed function; we emit a statepointed call to `dummy`.
    let caller_ty = ctx
      .void_type()
      .fn_type(&[gc_ptr_ty.into(), raw_ptr_ty.into()], false);
    let caller = module.add_function("caller", caller_ty, None);
    gc::set_default_gc_strategy(&caller).expect("set gc strategy");
    let entry = ctx.append_basic_block(caller, "entry");
    builder.position_at_end(entry);

    let gc_arg = caller.get_nth_param(0).expect("gc").into_pointer_value();
    gc_arg.set_name("gc");
    let raw_arg = caller.get_nth_param(1).expect("raw").into_pointer_value();
    raw_arg.set_name("raw");

    let callee = StatepointCallee::new(
      callee_fn.as_global_value().as_pointer_value(),
      callee_fn.get_type(),
    );
    let call_args: [BasicMetadataValueEnum<'_>; 2] = [gc_arg.into(), raw_arg.into()];

    let mut intrinsics = StatepointIntrinsics::new(&module);
    intrinsics.emit_statepoint_call(&builder, callee, &call_args, &[], None);

    builder.build_return(None).expect("ret");

    if let Err(err) = module.verify() {
      panic!(
        "LLVM module verification failed: {err}\n\nIR:\n{}",
        module.print_to_string()
      );
    }

    let ir = module.print_to_string().to_string();
    let statepoint_line = ir
      .lines()
      .find(|line| line.contains("gc.statepoint") && line.contains("\"gc-live\""))
      .expect("expected a gc.statepoint line with gc-live bundle");
    let live_sub = &statepoint_line[statepoint_line.find("\"gc-live\"").expect("gc-live bundle")..];
    assert!(
      live_sub.contains("%gc"),
      "expected gc pointer to appear in gc-live bundle: {statepoint_line}"
    );
    assert!(
      !live_sub.contains("%raw"),
      "raw pointers must not appear in gc-live bundle: {statepoint_line}"
    );
  }
}

// Inkwell's public API intentionally funnels all type/value construction through the
// `unsafe { ...::new(LLVMValueRef) }` constructors. Keep the `unsafe` localized here.
//
// If a future inkwell release changes this API, update this module accordingly.
#[allow(dead_code)]
fn _assert_inkwell_new_is_public<'ctx>(v: LLVMValueRef) {
  let _ = unsafe { BasicValueEnum::new(v) };
  let _ = unsafe { PointerValue::new(v) };
}
