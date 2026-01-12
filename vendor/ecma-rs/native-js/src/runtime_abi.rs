//! `native-js` ↔ `runtime-native` runtime entrypoints and call emission.
//!
//! ## GC correctness invariant: no may-GC wrapper frames
//!
//! LLVM stack maps are emitted only for *statepointed call sites* in functions marked with our GC
//! strategy. If generated code calls a helper/wrapper function which then calls a GC-triggering
//! runtime function, GC may run while that wrapper frame is on the stack — but the wrapper frame
//! would not necessarily have a corresponding stackmap record.
//!
//! To avoid this class of bugs, `native-js` treats runtime calls as either:
//! - **MayGC**: allocation/safepoints/collection. These calls must be the *actual* call site that is
//!   rewritten into `gc.statepoint` by LLVM.
//! - **NoGC**: operations like write barriers / keep-alive helpers. These must remain normal calls
//!   and are marked `"gc-leaf-function"` so LLVM does not statepoint them.
//!
//! ## ABI vs GC pointer types
//!
//! `runtime-native` exports a C ABI which uses normal pointers (`ptr` / addrspace(0)). Meanwhile,
//! LLVM's statepoint GC strategy identifies GC references via `ptr addrspace(1)`.
//!
//! `RuntimeAbi` keeps the runtime **extern declarations ABI-correct** (addrspace(0) pointers), and
//! then adapts at the call site:
//!
//! - For **MayGC** calls whose codegen signature uses `ptr addrspace(1)` (e.g. allocators returning
//!   GC pointers), we emit an **indirect call** to the raw runtime symbol using the addrspace(1)
//!   signature. This avoids `addrspacecast` from addrspace(1) (forbidden by our GC lint) while
//!   keeping the symbol's ABI stable (important for LTO).
//! - For **NoGC** calls with `ptr addrspace(1)` parameters (e.g. write barriers), we emit a small
//!   internal leaf wrapper (`rt_*_gc`) that performs the indirect call. Marking the wrapper as
//!   `"gc-leaf-function"` ensures calls remain non-statepointed.

use inkwell::attributes::AttributeLoc;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::BasicType as _;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, FunctionType, PointerType};
use inkwell::values::{BasicMetadataValueEnum, CallSiteValue, FunctionValue, PointerValue};
use inkwell::AddressSpace;

use runtime_native_abi::RtShapeId;

use crate::llvm::gc;
use crate::runtime_fn::{AbiTy, RuntimeFnAbi};
pub use crate::runtime_fn::{ArgRootingPolicy, RuntimeFn, RuntimeFnSpec};

// Keep `native-js`'s LLVM declarations in sync with the runtime ABI.
//
// `RtShapeId` is a `u32` in `runtime-native-abi` / `runtime-native/include/runtime_native.h`.
// If this ever changes, update the `i32` LLVM types in this module accordingly.
const _: [(); 4] = [(); core::mem::size_of::<RtShapeId>()];

// Runtime function signature metadata lives in `runtime_fn.rs` (`RuntimeFn::abi()`).

/// Declared runtime entrypoints and helpers for a module.
#[derive(Clone, Copy)]
pub struct RuntimeFns<'ctx> {
  // Raw runtime ABI symbols (addrspace(0)).
  pub rt_alloc: FunctionValue<'ctx>,
  pub rt_alloc_pinned: FunctionValue<'ctx>,
  pub rt_alloc_array: FunctionValue<'ctx>,
  pub rt_gc_safepoint: FunctionValue<'ctx>,
  pub rt_gc_safepoint_slow: FunctionValue<'ctx>,
  pub rt_gc_safepoint_relocate_h: FunctionValue<'ctx>,
  pub rt_gc_collect: FunctionValue<'ctx>,
  pub rt_gc_poll: FunctionValue<'ctx>,
  pub rt_write_barrier: FunctionValue<'ctx>,
  pub rt_write_barrier_range: FunctionValue<'ctx>,
  pub rt_keep_alive_gc_ref: FunctionValue<'ctx>,

  // Leaf wrappers for NoGC runtime fns that want addrspace(1) parameters.
  pub rt_write_barrier_gc: FunctionValue<'ctx>,
  pub rt_write_barrier_range_gc: FunctionValue<'ctx>,
  pub rt_keep_alive_gc_ref_gc: FunctionValue<'ctx>,

  // Parallel scheduler entrypoints are raw ABI (no GC pointer wrapper needed).
  pub rt_parallel_spawn: FunctionValue<'ctx>,
  pub rt_parallel_join: FunctionValue<'ctx>,
  pub rt_parallel_for: FunctionValue<'ctx>,
}

pub struct RuntimeAbi<'ctx, 'm> {
  context: &'ctx Context,
  module: &'m Module<'ctx>,
}

impl<'ctx, 'm> RuntimeAbi<'ctx, 'm> {
  pub fn new(context: &'ctx Context, module: &'m Module<'ctx>) -> Self {
    Self { context, module }
  }

  /// Backwards-compatible alias: historically this runtime ABI layer only generated wrappers.
  #[inline]
  pub fn ensure_wrappers(&self) -> RuntimeFns<'ctx> {
    self.declare_all()
  }

  /// Ensure all currently-known runtime entrypoints are declared in this module.
  ///
  /// Note: this declares the *raw runtime ABI* symbols (addrspace(0) pointers), plus any required
  /// leaf wrappers for NoGC entrypoints that use addrspace(1) parameters.
  pub fn declare_all(&self) -> RuntimeFns<'ctx> {
    // Ensure leaf wrappers exist so tests can assert on their presence.
    let rt_write_barrier_gc = self.get_or_define_leaf_wrapper(RuntimeFn::WriteBarrier);
    let rt_write_barrier_range_gc = self.get_or_define_leaf_wrapper(RuntimeFn::WriteBarrierRange);
    let rt_keep_alive_gc_ref_gc = self.get_or_define_leaf_wrapper(RuntimeFn::KeepAliveGcRef);

    RuntimeFns {
      rt_alloc: self.get_or_declare_raw(RuntimeFn::Alloc),
      rt_alloc_pinned: self.get_or_declare_raw(RuntimeFn::AllocPinned),
      rt_alloc_array: self.get_or_declare_raw(RuntimeFn::AllocArray),
      rt_gc_safepoint: self.get_or_declare_raw(RuntimeFn::GcSafepoint),
      rt_gc_safepoint_slow: self.get_or_declare_raw(RuntimeFn::GcSafepointSlow),
      rt_gc_safepoint_relocate_h: self.get_or_declare_raw(RuntimeFn::GcSafepointRelocateH),
      rt_gc_collect: self.get_or_declare_raw(RuntimeFn::GcCollect),
      rt_gc_poll: self.get_or_declare_raw(RuntimeFn::GcPoll),
      rt_write_barrier: self.get_or_declare_raw(RuntimeFn::WriteBarrier),
      rt_write_barrier_range: self.get_or_declare_raw(RuntimeFn::WriteBarrierRange),
      rt_keep_alive_gc_ref: self.get_or_declare_raw(RuntimeFn::KeepAliveGcRef),
      rt_write_barrier_gc,
      rt_write_barrier_range_gc,
      rt_keep_alive_gc_ref_gc,
      rt_parallel_spawn: self.get_or_declare_raw(RuntimeFn::ParallelSpawn),
      rt_parallel_join: self.get_or_declare_raw(RuntimeFn::ParallelJoin),
      rt_parallel_for: self.get_or_declare_raw(RuntimeFn::ParallelFor),
    }
  }

  fn ptr_raw(&self) -> PointerType<'ctx> {
    self.context.ptr_type(AddressSpace::default())
  }

  fn ptr_gc(&self) -> PointerType<'ctx> {
    self.context.ptr_type(gc::gc_address_space())
  }

  fn abi_ty_codegen_param(&self, ty: AbiTy) -> BasicMetadataTypeEnum<'ctx> {
    match ty {
      AbiTy::Void => panic!("void is not a valid parameter type"),
      AbiTy::I1 => self.context.bool_type().into(),
      AbiTy::I32 => self.context.i32_type().into(),
      AbiTy::I64 => self.context.i64_type().into(),
      AbiTy::RawPtr | AbiTy::GcHandle => self.ptr_raw().into(),
      AbiTy::GcPtr => self.ptr_gc().into(),
    }
  }

  fn abi_ty_codegen_ret(&self, ty: AbiTy) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
      AbiTy::Void => None,
      AbiTy::I1 => Some(self.context.bool_type().into()),
      AbiTy::I32 => Some(self.context.i32_type().into()),
      AbiTy::I64 => Some(self.context.i64_type().into()),
      AbiTy::RawPtr | AbiTy::GcHandle => Some(self.ptr_raw().into()),
      AbiTy::GcPtr => Some(self.ptr_gc().into()),
    }
  }

  fn abi_ty_runtime_param(&self, ty: AbiTy) -> BasicMetadataTypeEnum<'ctx> {
    // `runtime-native` ABI uses raw pointers for all pointer values.
    match ty {
      AbiTy::Void => panic!("void is not a valid parameter type"),
      AbiTy::I1 => self.context.bool_type().into(),
      AbiTy::I32 => self.context.i32_type().into(),
      AbiTy::I64 => self.context.i64_type().into(),
      AbiTy::RawPtr | AbiTy::GcHandle | AbiTy::GcPtr => self.ptr_raw().into(),
    }
  }

  fn abi_ty_runtime_ret(&self, ty: AbiTy) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
      AbiTy::Void => None,
      AbiTy::I1 => Some(self.context.bool_type().into()),
      AbiTy::I32 => Some(self.context.i32_type().into()),
      AbiTy::I64 => Some(self.context.i64_type().into()),
      AbiTy::RawPtr | AbiTy::GcHandle | AbiTy::GcPtr => Some(self.ptr_raw().into()),
    }
  }

  fn mark_gc_leaf(&self, func: FunctionValue<'ctx>) {
    // A string attribute with an empty value prints as `"gc-leaf-function"` in IR and instructs
    // LLVM's `rewrite-statepoints-for-gc` not to wrap calls to this function in a statepoint.
    let leaf = self.context.create_string_attribute("gc-leaf-function", "");
    func.add_attribute(AttributeLoc::Function, leaf);
  }

  fn codegen_fn_type(&self, abi: RuntimeFnAbi) -> FunctionType<'ctx> {
    let params: Vec<BasicMetadataTypeEnum<'ctx>> = abi
      .codegen_params
      .iter()
      .copied()
      .map(|t| self.abi_ty_codegen_param(t))
      .collect();
    match self.abi_ty_codegen_ret(abi.codegen_ret) {
      Some(ret) => ret.fn_type(&params, false),
      None => self.context.void_type().fn_type(&params, false),
    }
  }

  fn runtime_fn_type(&self, abi: RuntimeFnAbi) -> FunctionType<'ctx> {
    let params: Vec<BasicMetadataTypeEnum<'ctx>> = abi
      .runtime_params
      .iter()
      .copied()
      .map(|t| self.abi_ty_runtime_param(t))
      .collect();
    match self.abi_ty_runtime_ret(abi.runtime_ret) {
      Some(ret) => ret.fn_type(&params, false),
      None => self.context.void_type().fn_type(&params, false),
    }
  }

  /// Get or declare the raw runtime ABI entrypoint.
  ///
  /// For `!may_gc` entrypoints we apply `"gc-leaf-function"` to ensure LLVM does not rewrite calls
  /// to them into statepoints.
  pub fn get_or_declare_raw(&self, f: RuntimeFn) -> FunctionValue<'ctx> {
    let spec = f.spec();
    let abi = f.abi();

    let func = if let Some(existing) = self.module.get_function(spec.name) {
      existing
    } else {
      let fn_ty = self.runtime_fn_type(abi);
      self.module.add_function(spec.name, fn_ty, None)
    };

    if !spec.may_gc {
      self.mark_gc_leaf(func);
    }

    func
  }

  fn get_or_define_internal(
    &self,
    name: &str,
    ty: FunctionType<'ctx>,
    define_body: impl FnOnce(FunctionValue<'ctx>),
  ) -> FunctionValue<'ctx> {
    if let Some(existing) = self.module.get_function(name) {
      crate::stack_walking::apply_stack_walking_frame_attrs(self.context, existing);
      if existing.get_first_basic_block().is_some() {
        return existing;
      }
      define_body(existing);
      return existing;
    }

    let func = self.module.add_function(name, ty, Some(Linkage::Internal));
    crate::stack_walking::apply_stack_walking_frame_attrs(self.context, func);
    define_body(func);
    func
  }

  /// Produce a non-constant function pointer value for `raw` in the current function.
  ///
  /// This is required when we want to call `raw` with a *different* signature (e.g. returning
  /// `ptr addrspace(1)` instead of `ptr`). If the callee operand is a direct `@function`, LLVM will
  /// treat the call as a direct call and insist the callsite signature matches the function's
  /// declared type.
  fn load_indirect_callee_ptr(
    &self,
    builder: &Builder<'ctx>,
    raw: FunctionValue<'ctx>,
    hint: &str,
  ) -> PointerValue<'ctx> {
    let fn_ptr_ty = self.ptr_raw();
    let bb = builder
      .get_insert_block()
      .expect("builder must have an insertion block");
    let func = bb
      .get_parent()
      .expect("insertion block must have a parent function");
    let entry = func
      .get_first_basic_block()
      .expect("function must have an entry block");
    let entry_builder = self.context.create_builder();
    match entry.get_first_instruction() {
      Some(inst) => entry_builder.position_before(&inst),
      None => entry_builder.position_at_end(entry),
    }

    let slot = entry_builder
      .build_alloca(fn_ptr_ty, &format!("rt.fp.{hint}"))
      .expect("alloca runtime fn ptr slot");
    entry_builder
      .build_store(slot, raw.as_global_value().as_pointer_value())
      .expect("store runtime fn ptr");

    builder
      .build_load(fn_ptr_ty, slot, &format!("rt.fp.{hint}.ld"))
      .expect("load runtime fn ptr")
      .into_pointer_value()
  }

  fn get_or_define_leaf_wrapper(&self, f: RuntimeFn) -> FunctionValue<'ctx> {
    let spec = f.spec();
    let abi = f.abi();
    if spec.may_gc || abi.signatures_match() {
      // No wrapper needed.
      return self.get_or_declare_raw(f);
    }

    let wrapper_name = format!("{}_gc", spec.name);
    let wrapper_ty = self.codegen_fn_type(abi);

    self.get_or_define_internal(&wrapper_name, wrapper_ty, |func| {
      // Wrapper functions are leaf (NoGC) and must not be rewritten into statepoints.
      self.mark_gc_leaf(func);

      let builder = self.context.create_builder();
      let entry = self.context.append_basic_block(func, "entry");
      builder.position_at_end(entry);

      // Indirect-call the raw runtime symbol using the addrspace(1) signature so we don't need an
      // addrspacecast from addrspace(1) to addrspace(0) (which would hide GC pointers).
      let raw = self.get_or_declare_raw(f);
      let callee_ptr = self.load_indirect_callee_ptr(&builder, raw, &wrapper_name);

      let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(abi.codegen_params.len());
      for i in 0..abi.codegen_params.len() {
        args.push(func.get_nth_param(i as u32).expect("param").into());
      }

      let call = builder
        .build_indirect_call(wrapper_ty, callee_ptr, &args, "rt.call")
        .expect("build indirect call");
      crate::stack_walking::mark_call_notail(call);

      match self.abi_ty_codegen_ret(abi.codegen_ret) {
        None => {
          builder.build_return(None).expect("ret void");
        }
        Some(ret_ty) => {
          let ret = call
            .try_as_basic_value()
            .left()
            .unwrap_or_else(|| panic!("{wrapper_name} should return {ret_ty:?}"));
          builder.build_return(Some(&ret)).expect("ret value");
        }
      }
    })
  }

  /// Emit a call to a runtime function, enforcing GC-safety rules for its ABI.
  ///
  /// This emits a **normal call instruction** (not a `gc.statepoint`). For MayGC calls, LLVM's
  /// `rewrite-statepoints-for-gc` pass will rewrite the call into a statepoint in GC-managed
  /// functions.
  pub fn emit_runtime_call(
    &self,
    builder: &Builder<'ctx>,
    f: RuntimeFn,
    args: &[BasicMetadataValueEnum<'ctx>],
    name: &str,
  ) -> Result<CallSiteValue<'ctx>, RuntimeCallError> {
    let abi = f.abi();
    let spec = f.spec();
    validate_runtime_call_abi(spec, Some(abi), args)?;

    // NoGC + signature mismatch: call the leaf wrapper.
    if !spec.may_gc && !abi.signatures_match() {
      let wrapper = self.get_or_define_leaf_wrapper(f);
      let call = builder
        .build_call(wrapper, args, name)
        .map_err(|e| RuntimeCallError::BuildCall {
          name: spec.name,
          message: e.to_string(),
        })?;
      crate::stack_walking::mark_call_notail(call);
      return Ok(call);
    }

    let raw = self.get_or_declare_raw(f);

    let call = if abi.signatures_match() {
      builder
        .build_call(raw, args, name)
        .map_err(|e| RuntimeCallError::BuildCall {
          name: spec.name,
          message: e.to_string(),
        })?
    } else {
      // MayGC call with an addrspace(1) signature mismatch: emit an indirect call to the raw symbol.
      let codegen_ty = self.codegen_fn_type(abi);
      let callee_ptr = self.load_indirect_callee_ptr(builder, raw, spec.name);
      builder
        .build_indirect_call(codegen_ty, callee_ptr, args, name)
        .map_err(|e| RuntimeCallError::BuildCall {
          name: spec.name,
          message: e.to_string(),
        })?
    };
    crate::stack_walking::mark_call_notail(call);
    Ok(call)
  }

  // Parallel scheduler entrypoints are now described in `RuntimeFn` so they share the same
  // GC-safety metadata validation as other runtime entrypoints.
}

// -----------------------------------------------------------------------------
// Runtime call ABI validation
// -----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RuntimeCallError {
  #[error(
    "invalid runtime ABI for `{name}`: marked may_gc=true but takes {gc_ptr_args} GC pointer arg(s); \
     LLVM statepoints/stackmaps do not describe runtime-native frames, so MayGC runtime fns must be \
     pointer-free or use handles (ArgRootingPolicy::RuntimeRootsPointers)"
  )]
  MayGcWithGcPointerArgs { name: &'static str, gc_ptr_args: usize },

  #[error("failed to build call to runtime function `{name}`: {message}")]
  BuildCall { name: &'static str, message: String },
}

fn validate_runtime_call_abi<'ctx>(
  spec: RuntimeFnSpec,
  abi: Option<RuntimeFnAbi>,
  args: &[BasicMetadataValueEnum<'ctx>],
) -> Result<(), RuntimeCallError> {
  // Don't rely solely on the manually-maintained `gc_ptr_args` metadata; also validate against the
  // actual call argument types. This makes it hard to accidentally construct an unsound MayGC
  // runtime call by forgetting to update the registry.
  let actual_gc_ptr_args = args
    .iter()
    .filter(|arg| match arg {
      BasicMetadataValueEnum::PointerValue(ptr) => {
        ptr.get_type().get_address_space() == gc::gc_address_space()
      }
      _ => false,
    })
    .count();
  debug_assert_eq!(
    actual_gc_ptr_args, spec.gc_ptr_args,
    "runtime fn spec mismatch for `{}`: spec.gc_ptr_args={} but call has {} ptr addrspace(1) arg(s)",
    spec.name, spec.gc_ptr_args, actual_gc_ptr_args
  );

  // Similar defensive check for handle ABI (`GcHandle = *mut *mut u8`) arguments.
  //
  // We can't reliably validate the pointee type in opaque-pointer mode, so we instead enforce a
  // simple convention:
  //
  // - If we have an explicit `RuntimeFnAbi`, validate handle args at the positions marked
  //   `AbiTy::GcHandle`.
  // - Otherwise (legacy callers), assume handle args appear at the end of the argument list.
  //
  // In both cases, handle args must be addrspace(0) pointers (i.e. not `ptr addrspace(1)` GC
  // references).
  if spec.gc_handle_args > 0 {
    let validation_kind = if abi.is_some() {
      "ABI signature"
    } else {
      "legacy last-args convention"
    };
    let actual_handle_args = match abi {
      Some(abi) => {
        debug_assert_eq!(
          abi.codegen_params.len(),
          args.len(),
          "runtime fn spec mismatch for `{}`: ABI expects {} arg(s) but call has {}",
          spec.name,
          abi.codegen_params.len(),
          args.len()
        );
        abi
          .codegen_params
          .iter()
          .zip(args.iter())
          .filter(|(&ty, arg)| {
            if ty != AbiTy::GcHandle {
              return false;
            }
            match arg {
              BasicMetadataValueEnum::PointerValue(ptr) => {
                ptr.get_type().get_address_space() == AddressSpace::default()
              }
              _ => false,
            }
          })
          .count()
      }
      None => {
        let handle_slice = &args[args.len().saturating_sub(spec.gc_handle_args)..];
        handle_slice
          .iter()
          .filter(|arg| match arg {
            BasicMetadataValueEnum::PointerValue(ptr) => {
              ptr.get_type().get_address_space() == AddressSpace::default()
            }
            _ => false,
          })
          .count()
      }
    };
    debug_assert_eq!(
      actual_handle_args, spec.gc_handle_args,
      "runtime fn spec mismatch for `{}`: spec.gc_handle_args={} but {validation_kind} observed {} handle arg(s) (addrspace(0) pointers)",
      spec.name, spec.gc_handle_args, actual_handle_args
    );
  }

  if spec.may_gc
    && actual_gc_ptr_args > 0
    && spec.arg_rooting != ArgRootingPolicy::RuntimeRootsPointers
  {
    return Err(RuntimeCallError::MayGcWithGcPointerArgs {
      name: spec.name,
      gc_ptr_args: actual_gc_ptr_args,
    });
  }

  Ok(())
}

/// Emit a call to a runtime function, enforcing GC-safety rules for its ABI.
pub fn emit_runtime_call<'ctx>(
  builder: &Builder<'ctx>,
  callee: FunctionValue<'ctx>,
  spec: RuntimeFnSpec,
  args: &[BasicMetadataValueEnum<'ctx>],
  name: &str,
) -> Result<CallSiteValue<'ctx>, RuntimeCallError> {
  validate_runtime_call_abi(spec, None, args)?;

  let call = builder.build_call(callee, args, name).map_err(|e| RuntimeCallError::BuildCall {
    name: spec.name,
    message: e.to_string(),
  })?;
  crate::stack_walking::mark_call_notail(call);
  Ok(call)
}
