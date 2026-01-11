use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{FunctionType, PointerType};
use inkwell::values::{BasicMetadataValueEnum, CallSiteValue, FunctionValue};
use inkwell::AddressSpace;

use crate::llvm::gc;
pub use crate::runtime_fn::{ArgRootingPolicy, RuntimeFn, RuntimeFnSpec};

/// `native-js` ↔ `runtime-native` ABI boundary helpers.
///
/// # Problem
/// LLVM's statepoint GC support expects *GC references* to be typed as
/// `ptr addrspace(1)`, but our Rust runtime exports C ABI functions using normal
/// pointers (`*mut u8` / `ptr` == addrspace(0)).
///
/// To keep the runtime ABI stable *and* keep LLVM's GC relocation machinery happy, we:
///
/// 1. Declare the runtime entrypoints using addrspace(0) pointer types (matching Rust's ABI).
/// 2. Emit internal wrapper functions (`*_gc`) that expose addrspace(1) signatures to the rest of
///    the generated code.
///
/// Critically, the wrappers **must not** `addrspacecast` GC pointers from addrspace(1) back to
/// addrspace(0) because that would "hide" them from `rewrite-statepoints-for-gc`.
///
/// Instead, wrappers call the raw runtime symbol via an *indirect call* using the addrspace(1)
/// signature. On our targets the address-space annotation does not affect the machine ABI (it's a
/// type-system distinction only), but it keeps GC pointers visible to LLVM's statepoint passes.
///
/// Generated code should exclusively call the `*_gc` wrappers and never call the
/// raw runtime functions directly with GC pointers.
///
/// These wrappers are intentionally **not** marked as GC-managed functions (`gc "coreclr"`), so
/// they are outside the GC pointer discipline lint (which only applies to GC-managed functions).
///
/// # Naming
/// We intentionally keep the runtime's exported symbol names (`rt_*`) as the raw
/// externs, and suffix wrappers with `_gc` (e.g. `rt_alloc_gc`). This avoids any
/// need to rename the Rust runtime ABI.
pub struct RuntimeAbi<'ctx, 'm> {
  context: &'ctx Context,
  module: &'m Module<'ctx>,
  builder: Builder<'ctx>,
}

impl<'ctx, 'm> RuntimeAbi<'ctx, 'm> {
  pub fn new(context: &'ctx Context, module: &'m Module<'ctx>) -> Self {
    Self {
      context,
      module,
      builder: context.create_builder(),
    }
  }

  /// Ensure the common runtime wrappers exist in this LLVM module.
  pub fn ensure_wrappers(&self) -> RuntimeFns<'ctx> {
    RuntimeFns {
      rt_alloc_gc: self.rt_alloc_gc(),
      rt_alloc_pinned_gc: self.rt_alloc_pinned_gc(),
      rt_gc_safepoint_gc: self.rt_gc_safepoint_gc(),
      rt_gc_collect_gc: self.rt_gc_collect_gc(),
      rt_write_barrier_gc: self.rt_write_barrier_gc(),
      rt_keep_alive_gc_ref_gc: self.rt_keep_alive_gc_ref_gc(),
      rt_parallel_spawn_raw: self.rt_parallel_spawn_raw(),
      rt_parallel_join_raw: self.rt_parallel_join_raw(),
      rt_parallel_for_raw: self.rt_parallel_for_raw(),
    }
  }

  fn ptr_raw(&self) -> PointerType<'ctx> {
    self.context.ptr_type(AddressSpace::default())
  }

  fn ptr_gc(&self) -> PointerType<'ctx> {
    self.context.ptr_type(gc::gc_address_space())
  }

  fn get_or_declare(&self, name: &str, ty: FunctionType<'ctx>) -> FunctionValue<'ctx> {
    if let Some(existing) = self.module.get_function(name) {
      return existing;
    }
    self.module.add_function(name, ty, None)
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

  // -----------------------------------------------------------------------------
  // Raw runtime extern declarations (addrspace(0))
  // -----------------------------------------------------------------------------

  fn rt_alloc_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_alloc(size: usize, shape: RtShapeId) -> *mut u8`
    let i64_ty = self.context.i64_type();
    let i32_ty = self.context.i32_type();
    let fn_ty = self
      .ptr_raw()
      .fn_type(&[i64_ty.into(), i32_ty.into()], false);
    self.get_or_declare("rt_alloc", fn_ty)
  }

  fn rt_alloc_pinned_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_alloc_pinned(size: usize, shape: RtShapeId) -> *mut u8`
    let i64_ty = self.context.i64_type();
    let i32_ty = self.context.i32_type();
    let fn_ty = self
      .ptr_raw()
      .fn_type(&[i64_ty.into(), i32_ty.into()], false);
    self.get_or_declare("rt_alloc_pinned", fn_ty)
  }

  fn rt_gc_safepoint_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);
    self.get_or_declare("rt_gc_safepoint", fn_ty)
  }

  fn rt_gc_collect_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);
    self.get_or_declare("rt_gc_collect", fn_ty)
  }

  fn rt_write_barrier_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_raw().into(), self.ptr_raw().into()], false);
    self.get_or_declare("rt_write_barrier", fn_ty)
  }

  fn rt_keep_alive_gc_ref_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_raw().into()], false);
    self.get_or_declare("rt_keep_alive_gc_ref", fn_ty)
  }

  fn rt_parallel_spawn_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId`
    let i64_ty = self.context.i64_type();
    // In LLVM's opaque-pointer mode, function pointers are simply `ptr`.
    let fn_ty = i64_ty.fn_type(&[self.ptr_raw().into(), self.ptr_raw().into()], false);
    self.get_or_declare("rt_parallel_spawn", fn_ty)
  }

  fn rt_parallel_join_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_parallel_join(tasks: *const TaskId, count: usize)`
    let i64_ty = self.context.i64_type();
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_raw().into(), i64_ty.into()], false);
    self.get_or_declare("rt_parallel_join", fn_ty)
  }

  fn rt_parallel_for_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_parallel_for(start: usize, end: usize, body: extern "C" fn(usize, *mut u8), data: *mut u8)`
    let i64_ty = self.context.i64_type();
    let fn_ty = self.context.void_type().fn_type(
      &[
        i64_ty.into(),
        i64_ty.into(),
        // In LLVM's opaque-pointer mode, function pointers are simply `ptr`.
        self.ptr_raw().into(),
        self.ptr_raw().into(),
      ],
      false,
    );
    self.get_or_declare("rt_parallel_for", fn_ty)
  }

  // -----------------------------------------------------------------------------
  // GC wrappers (addrspace(1))
  // -----------------------------------------------------------------------------

  fn rt_alloc_gc(&self) -> FunctionValue<'ctx> {
    let i64_ty = self.context.i64_type();
    let i32_ty = self.context.i32_type();
    let fn_ty = self
      .ptr_gc()
      .fn_type(&[i64_ty.into(), i32_ty.into()], false);

    self.get_or_define_internal("rt_alloc_gc", fn_ty, |func| {
      let raw = self.rt_alloc_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let size = func.get_nth_param(0).expect("size").into_int_value();
      let shape = func.get_nth_param(1).expect("shape").into_int_value();

      // Indirect-call the raw symbol using the addrspace(1) signature so we don't need an
      // addrspacecast (which would hide GC pointers from LLVM's statepoint rewriting).
      let fn_ptr_ty = self.ptr_raw();
      let fp_slot = self
        .builder
        .build_alloca(fn_ptr_ty, "rt_alloc_fp")
        .expect("alloca rt_alloc fp");
      self
        .builder
        .build_store(fp_slot, raw.as_global_value().as_pointer_value())
        .expect("store rt_alloc fp");
      let fp = self
        .builder
        .build_load(fn_ptr_ty, fp_slot, "rt_alloc_fp")
        .expect("load rt_alloc fp")
        .into_pointer_value();

      let call = self
        .builder
        .build_indirect_call(fn_ty, fp, &[size.into(), shape.into()], "gc_ptr")
        .expect("call rt_alloc via indirect call");
      crate::stack_walking::mark_call_notail(call);
      let gc_ptr = call
        .try_as_basic_value()
        .left()
        .expect("rt_alloc returns ptr")
        .into_pointer_value();

      self
        .builder
        .build_return(Some(&gc_ptr))
        .expect("return gc ptr");
    })
  }

  fn rt_alloc_pinned_gc(&self) -> FunctionValue<'ctx> {
    let i64_ty = self.context.i64_type();
    let i32_ty = self.context.i32_type();
    let fn_ty = self
      .ptr_gc()
      .fn_type(&[i64_ty.into(), i32_ty.into()], false);

    self.get_or_define_internal("rt_alloc_pinned_gc", fn_ty, |func| {
      let raw = self.rt_alloc_pinned_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let size = func.get_nth_param(0).expect("size").into_int_value();
      let shape = func.get_nth_param(1).expect("shape").into_int_value();

      let fn_ptr_ty = self.ptr_raw();
      let fp_slot = self
        .builder
        .build_alloca(fn_ptr_ty, "rt_alloc_pinned_fp")
        .expect("alloca rt_alloc_pinned fp");
      self
        .builder
        .build_store(fp_slot, raw.as_global_value().as_pointer_value())
        .expect("store rt_alloc_pinned fp");
      let fp = self
        .builder
        .build_load(fn_ptr_ty, fp_slot, "rt_alloc_pinned_fp")
        .expect("load rt_alloc_pinned fp")
        .into_pointer_value();

      let call = self
        .builder
        .build_indirect_call(fn_ty, fp, &[size.into(), shape.into()], "gc_ptr")
        .expect("call rt_alloc_pinned via indirect call");
      crate::stack_walking::mark_call_notail(call);
      let gc_ptr = call
        .try_as_basic_value()
        .left()
        .expect("rt_alloc_pinned returns ptr")
        .into_pointer_value();

      self
        .builder
        .build_return(Some(&gc_ptr))
        .expect("return gc ptr");
    })
  }

  fn rt_gc_safepoint_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);

    self.get_or_define_internal("rt_gc_safepoint_gc", fn_ty, |func| {
      let raw = self.rt_gc_safepoint_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);
      let call = self.builder.build_call(raw, &[], "").expect("call rt_gc_safepoint");
      crate::stack_walking::mark_call_notail(call);
      self.builder.build_return(None).expect("return void");
    })
  }

  fn rt_gc_collect_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);

    self.get_or_define_internal("rt_gc_collect_gc", fn_ty, |func| {
      let raw = self.rt_gc_collect_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);
      let _ = self
        .builder
        .build_call(raw, &[], "")
        .expect("call rt_gc_collect");
      self.builder.build_return(None).expect("return void");
    })
  }

  fn rt_write_barrier_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_gc().into(), self.ptr_gc().into()], false);

    self.get_or_define_internal("rt_write_barrier_gc", fn_ty, |func| {
      let raw = self.rt_write_barrier_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let obj_gc = func.get_nth_param(0).expect("obj").into_pointer_value();
      let field_gc = func.get_nth_param(1).expect("field").into_pointer_value();

      // As with allocation, avoid addrspacecasting GC pointers back to addrspace(0); call the raw
      // runtime symbol via an indirect call using the addrspace(1) signature.
      let fn_ptr_ty = self.ptr_raw();
      let fp_slot = self
        .builder
        .build_alloca(fn_ptr_ty, "rt_write_barrier_fp")
        .expect("alloca rt_write_barrier fp");
      self
        .builder
        .build_store(fp_slot, raw.as_global_value().as_pointer_value())
        .expect("store rt_write_barrier fp");
      let fp = self
        .builder
        .build_load(fn_ptr_ty, fp_slot, "rt_write_barrier_fp")
        .expect("load rt_write_barrier fp")
        .into_pointer_value();

      let call = self
        .builder
        .build_indirect_call(fn_ty, fp, &[obj_gc.into(), field_gc.into()], "")
        .expect("call rt_write_barrier via indirect call");
      crate::stack_walking::mark_call_notail(call);

      self.builder.build_return(None).expect("return void");
    })
  }

  fn rt_keep_alive_gc_ref_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_gc().into()], false);

    self.get_or_define_internal("rt_keep_alive_gc_ref_gc", fn_ty, |func| {
      let raw = self.rt_keep_alive_gc_ref_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let gc_ref = func.get_nth_param(0).expect("gc_ref").into_pointer_value();

      // Avoid addrspacecasting the GC pointer back to addrspace(0); call the raw runtime symbol via
      // an indirect call using the addrspace(1) signature.
      let fn_ptr_ty = self.ptr_raw();
      let fp_slot = self
        .builder
        .build_alloca(fn_ptr_ty, "rt_keep_alive_gc_ref_fp")
        .expect("alloca rt_keep_alive_gc_ref fp");
      self
        .builder
        .build_store(fp_slot, raw.as_global_value().as_pointer_value())
        .expect("store rt_keep_alive_gc_ref fp");
      let fp = self
        .builder
        .build_load(fn_ptr_ty, fp_slot, "rt_keep_alive_gc_ref_fp")
        .expect("load rt_keep_alive_gc_ref fp")
        .into_pointer_value();

      let _ = self
        .builder
        .build_indirect_call(fn_ty, fp, &[gc_ref.into()], "")
        .expect("call rt_keep_alive_gc_ref via indirect call");

      self.builder.build_return(None).expect("return void");
    })
  }
}

/// Handles to the runtime wrapper functions defined in an LLVM module.
#[derive(Clone, Copy)]
pub struct RuntimeFns<'ctx> {
  pub rt_alloc_gc: FunctionValue<'ctx>,
  pub rt_alloc_pinned_gc: FunctionValue<'ctx>,
  pub rt_gc_safepoint_gc: FunctionValue<'ctx>,
  pub rt_gc_collect_gc: FunctionValue<'ctx>,
  pub rt_write_barrier_gc: FunctionValue<'ctx>,
  pub rt_keep_alive_gc_ref_gc: FunctionValue<'ctx>,
  pub rt_parallel_spawn_raw: FunctionValue<'ctx>,
  pub rt_parallel_join_raw: FunctionValue<'ctx>,
  pub rt_parallel_for_raw: FunctionValue<'ctx>,
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

/// Emit a call to a runtime function, enforcing GC-safety rules for its ABI.
pub fn emit_runtime_call<'ctx>(
  builder: &Builder<'ctx>,
  callee: FunctionValue<'ctx>,
  spec: RuntimeFnSpec,
  args: &[BasicMetadataValueEnum<'ctx>],
  name: &str,
) -> Result<CallSiteValue<'ctx>, RuntimeCallError> {
  // Don't rely solely on the manually-maintained `gc_ptr_args` metadata; also validate against the
  // actual call argument types. This makes it hard to accidentally construct an unsound MayGC
  // runtime call by forgetting to update the registry.
  let actual_gc_ptr_args = args
    .iter()
    .filter(|arg| match arg {
      BasicMetadataValueEnum::PointerValue(ptr) => ptr.get_type().get_address_space() == gc::gc_address_space(),
      _ => false,
    })
    .count();
  debug_assert_eq!(
    actual_gc_ptr_args, spec.gc_ptr_args,
    "runtime fn spec mismatch for `{}`: spec.gc_ptr_args={} but call has {} ptr addrspace(1) arg(s)",
    spec.name, spec.gc_ptr_args, actual_gc_ptr_args
  );

  if spec.may_gc
    && actual_gc_ptr_args > 0
    && spec.arg_rooting != ArgRootingPolicy::RuntimeRootsPointers
  {
    return Err(RuntimeCallError::MayGcWithGcPointerArgs {
      name: spec.name,
      gc_ptr_args: actual_gc_ptr_args,
    });
  }

  builder
    .build_call(callee, args, name)
    .map_err(|e| RuntimeCallError::BuildCall {
      name: spec.name,
      message: e.to_string(),
    })
}

impl<'ctx, 'm> RuntimeAbi<'ctx, 'm> {
  pub fn runtime_fn(&self, f: RuntimeFn) -> FunctionValue<'ctx> {
    match f {
      RuntimeFn::Alloc => self.rt_alloc_gc(),
      RuntimeFn::AllocPinned => self.rt_alloc_pinned_gc(),
      RuntimeFn::GcSafepoint => self.rt_gc_safepoint_gc(),
      RuntimeFn::GcCollect => self.rt_gc_collect_gc(),
      RuntimeFn::WriteBarrier => self.rt_write_barrier_gc(),
    }
  }

  /// Convenience wrapper around [`emit_runtime_call`] for known runtime functions.
  pub fn emit_runtime_call(
    &self,
    builder: &Builder<'ctx>,
    f: RuntimeFn,
    args: &[BasicMetadataValueEnum<'ctx>],
    name: &str,
  ) -> Result<CallSiteValue<'ctx>, RuntimeCallError> {
    let callee = self.runtime_fn(f);
    emit_runtime_call(builder, callee, f.spec(), args, name)
  }
}
