/// Metadata about runtime functions callable from compiled code.
///
/// This module is the single registry for runtime entrypoints (Task 315). It encodes the GC-safety
/// properties needed by codegen:
///
/// - Whether a call may trigger GC (`may_gc`).
/// - Whether the ABI contains raw GC pointers (`gc_ptr_args`), which is **unsound** for `may_gc`
///   runtime functions unless the runtime provides its own argument-rooting mechanism.
/// - Whether the ABI contains GC **handles** (`gc_handle_args`), i.e. pointer-to-slot arguments
///   (`GcHandle = *mut *mut u8`). Handle args are allowed for `may_gc` functions because the runtime
///   can reload `*handle` after a safepoint/GC.
///
/// See `native-js/docs/llvm_gc_strategy.md` for the full rationale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeFn {
  // -----------------------------------------------------------------------------
  // Thread lifecycle / mutator registration
  // -----------------------------------------------------------------------------
  //
  // These entrypoints may allocate thread-local runtime state and/or block on GC-aware locks.
  // Calls from compiled code must therefore be eligible for statepoint rewriting.

  /// Register the current OS thread with the runtime.
  ///
  /// `rt_thread_init(kind: u32)`
  ThreadInit,
  /// Unregister the current OS thread from the runtime.
  ///
  /// `rt_thread_deinit()`
  ThreadDeinit,
  /// Register the current OS thread and return a runtime-assigned thread id.
  ///
  /// `rt_thread_register(kind: u32) -> u64`
  ThreadRegister,
  /// Unregister the current OS thread previously registered via [`RuntimeFn::ThreadRegister`].
  ///
  /// `rt_thread_unregister()`
  ThreadUnregister,
  /// Mark/unmark the current thread as "parked".
  ///
  /// `rt_thread_set_parked(parked: bool)`
  ThreadSetParked,

  // -----------------------------------------------------------------------------
  // Memory / shape tables
  // -----------------------------------------------------------------------------

  /// Register the global shape table used by [`runtime_native_abi::RtShapeId`].
  ///
  /// `rt_register_shape_table(ptr: *const RtShapeDescriptor, len: usize)`
  RegisterShapeTable,
  /// Allocation entrypoint: always may trigger GC.
  Alloc,
  /// Pinned allocation entrypoint: always may trigger GC.
  AllocPinned,
  /// Allocate a GC-managed array object (`rt_alloc_array`).
  ///
  /// Arrays have a dynamic size derived from their header, so they use a shared
  /// `TypeDescriptor` and are special-cased by the GC for tracing/sizing.
  AllocArray,
  /// Register a global/static root slot (`usize*`) with the runtime.
  ///
  /// This is used for GC-managed pointers stored in global/static memory (e.g. TypeScript module
  /// globals, runtime singletons).
  ///
  /// Marked as `may_gc` even though it does not allocate in the GC heap: it can contend on a
  /// GC-aware mutex and enter a GC-safe region while waiting, during which a stop-the-world GC may
  /// run. Compiled code must therefore treat the callsite as a potential safepoint and ensure stack
  /// maps exist at the return address.
  GlobalRootRegister,
  /// Unregister a global/static root slot previously registered via [`RuntimeFn::GlobalRootRegister`].
  ///
  /// Like registration, this can block on GC-aware locks and must be treated as `may_gc` for
  /// stackmap correctness.
  GlobalRootUnregister,
  /// Convenience GC safepoint poll (`rt_gc_safepoint()`).
  ///
  /// Compiled code should prefer `GcSafepointSlow` + `RT_GC_EPOCH` polling so the runtime can
  /// capture the managed callsite context.
  GcSafepoint,
  /// Slow path for explicit safepoint polling:
  /// `rt_gc_safepoint_slow(epoch: u64)`.
  ///
  /// This is used by backedge polling fast paths (see `codegen::safepoint`).
  GcSafepointSlow,
  /// Enter a GC safepoint and return the relocated pointer stored in a handle slot.
  GcSafepointRelocateH,
  /// Forces a GC cycle.
  GcCollect,
  /// Poll-only helper (`rt_gc_poll() -> bool`).
  ///
  /// This must not allocate or safepoint.
  GcPoll,
  /// Generational write barrier (must not allocate / GC).
  WriteBarrier,
  /// Generational write barrier for a contiguous range (must not allocate / GC).
  WriteBarrierRange,
  /// Keep a GC object alive until after the last use of a derived raw pointer.
  ///
  /// This prevents the compiler from considering a GC reference dead while a raw pointer derived
  /// from it is still in use (e.g. an `ArrayBuffer` backing store pointer).
  ///
  /// The native runtime uses this to ensure owner objects remain live when compiled code forms
  /// derived/interior pointers that are used after a safepoint (Task 385).
  ///
  /// Contract: must not allocate or trigger GC.
  KeepAliveGcRef,

  // -----------------------------------------------------------------------------
  // Parallel scheduler (worker pool)
  // -----------------------------------------------------------------------------
  //
  // These entrypoints do not take GC pointers by default, but they are still treated as **MayGC**
  // because:
  // - they may allocate scheduler state, and/or
  // - they may block, during which a stop-the-world GC can occur.
  //
  // Therefore, calls from GC-managed code must be eligible for statepoint rewriting (i.e. these
  // symbols must not be marked `"gc-leaf-function"`).

  /// `rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId`
  ParallelSpawn,
  /// `rt_parallel_join(tasks: *const TaskId, count: usize)`
  ParallelJoin,
  /// `rt_parallel_for(start: usize, end: usize, body: extern "C" fn(usize, *mut u8), data: *mut u8)`
  ParallelFor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbiTy {
  Void,
  I1,
  I32,
  I64,
  /// Raw runtime pointer (addrspace(0)).
  RawPtr,
  /// Handle ABI pointer (`GcHandle = *mut *mut u8`, i.e. pointer-to-slot).
  ///
  /// In LLVM IR this is represented as a normal `ptr` (addrspace(0)), but it has a distinct
  /// *semantic* meaning from [`AbiTy::RawPtr`]: the pointee is a caller-owned root slot containing a
  /// relocatable GC pointer.
  ///
  /// This is used for `may_gc` runtime entrypoints that need GC pointer arguments: passing handles
  /// keeps the runtime's own stack/registers free of raw GC pointers, while still allowing the GC to
  /// update the caller's slot in-place.
  GcHandle,
  /// GC pointer in generated code (addrspace(1)).
  GcPtr,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeFnAbi {
  pub runtime_ret: AbiTy,
  pub runtime_params: &'static [AbiTy],
  pub codegen_ret: AbiTy,
  pub codegen_params: &'static [AbiTy],
}

impl RuntimeFnAbi {
  pub fn signatures_match(self) -> bool {
    self.runtime_ret == self.codegen_ret && self.runtime_params == self.codegen_params
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcEffect {
  /// Guaranteed not to trigger GC (leaf, no allocation, no safepoint polls).
  NoGc,
  /// May trigger GC.
  MayGc,
}

/// Policy for how runtime functions handle GC pointer arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgRootingPolicy {
  /// Default policy: runtime functions that may GC must not accept any raw GC pointers.
  ///
  /// Rationale: LLVM statepoints/stackmaps do not describe Rust/C runtime frames, so if the runtime
  /// function triggers a GC while it has GC pointer arguments in its own native stack/registers,
  /// those pointers will not be traced or relocated.
  NoGcPointersAllowedIfMayGc,
  /// The runtime guarantees it "roots" GC pointer arguments for the duration of the call (e.g.
  /// shadow stack, handles, pinning, or an equivalent mechanism).
  RuntimeRootsPointers,
}

impl Default for ArgRootingPolicy {
  fn default() -> Self {
    Self::NoGcPointersAllowedIfMayGc
  }
}

/// Metadata describing a runtime function's GC-safety contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFnSpec {
  pub name: &'static str,
  /// Whether this runtime function may allocate / safepoint / trigger GC.
  pub may_gc: bool,
  /// Number of arguments that are GC-managed pointers (i.e. raw pointers that refer to GC objects).
  pub gc_ptr_args: usize,
  /// Number of arguments that are GC handle pointers (`*mut *mut u8`) referencing caller-owned GC
  /// root slots.
  pub gc_handle_args: usize,
  pub arg_rooting: ArgRootingPolicy,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeFnDecl {
  pub spec: RuntimeFnSpec,
  pub abi: RuntimeFnAbi,
}

impl RuntimeFn {
  pub(crate) const fn decl(self) -> RuntimeFnDecl {
    match self {
      RuntimeFn::ThreadInit => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_thread_init",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I32],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I32],
        },
      },
      RuntimeFn::ThreadDeinit => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_thread_deinit",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[],
          codegen_ret: AbiTy::Void,
          codegen_params: &[],
        },
      },
      RuntimeFn::ThreadRegister => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_thread_register",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::I32],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::I32],
        },
      },
      RuntimeFn::ThreadUnregister => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_thread_unregister",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[],
          codegen_ret: AbiTy::Void,
          codegen_params: &[],
        },
      },
      RuntimeFn::ThreadSetParked => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_thread_set_parked",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I1],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I1],
        },
      },
      RuntimeFn::RegisterShapeTable => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_register_shape_table",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::Alloc => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_alloc",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64, AbiTy::I32],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64, AbiTy::I32],
        },
      },
      RuntimeFn::AllocPinned => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_alloc_pinned",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64, AbiTy::I32],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64, AbiTy::I32],
        },
      },
      RuntimeFn::AllocArray => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_alloc_array",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64, AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64, AbiTy::I64],
        },
      },
      RuntimeFn::GlobalRootRegister => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_global_root_register",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::RawPtr],
        },
      },
      RuntimeFn::GlobalRootUnregister => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_global_root_unregister",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::RawPtr],
        },
      },
      RuntimeFn::GcSafepoint => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_gc_safepoint",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[],
          codegen_ret: AbiTy::Void,
          codegen_params: &[],
        },
      },
      RuntimeFn::GcSafepointSlow => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_gc_safepoint_slow",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::GcSafepointRelocateH => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_gc_safepoint_relocate_h",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 1,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::GcHandle],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::GcHandle],
        },
      },
      RuntimeFn::GcCollect => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_gc_collect",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[],
          codegen_ret: AbiTy::Void,
          codegen_params: &[],
        },
      },
      RuntimeFn::GcPoll => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_gc_poll",
          may_gc: false,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I1,
          runtime_params: &[],
          codegen_ret: AbiTy::I1,
          codegen_params: &[],
        },
      },
      RuntimeFn::WriteBarrier => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_write_barrier",
          may_gc: false,
          gc_ptr_args: 2,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr, AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr, AbiTy::GcPtr],
        },
      },
      RuntimeFn::WriteBarrierRange => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_write_barrier_range",
          may_gc: false,
          gc_ptr_args: 2,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr, AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr, AbiTy::GcPtr, AbiTy::I64],
        },
      },
      RuntimeFn::KeepAliveGcRef => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_keep_alive_gc_ref",
          may_gc: false,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::ParallelSpawn => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_parallel_spawn",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr, AbiTy::RawPtr],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr, AbiTy::RawPtr],
        },
      },
      RuntimeFn::ParallelJoin => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_parallel_join",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::ParallelFor => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_parallel_for",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I64, AbiTy::I64, AbiTy::RawPtr, AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I64, AbiTy::I64, AbiTy::RawPtr, AbiTy::RawPtr],
        },
      },
    }
  }

  pub fn llvm_name(self) -> &'static str {
    self.spec().name
  }

  pub fn gc_effect(self) -> GcEffect {
    if self.spec().may_gc {
      GcEffect::MayGc
    } else {
      GcEffect::NoGc
    }
  }

  pub const fn spec(self) -> RuntimeFnSpec {
    self.decl().spec
  }

  pub(crate) const fn abi(self) -> RuntimeFnAbi {
    self.decl().abi
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  
  fn count_codegen_params(abi: RuntimeFnAbi, ty: AbiTy) -> usize {
    abi.codegen_params.iter().copied().filter(|&t| t == ty).count()
  }
 
  #[test]
  fn runtime_fn_registry_metadata_matches_abi() {
    // When adding a runtime entrypoint, keep the GC-safety metadata and ABI signature metadata in
    // sync. This test is intentionally internal (unit test) so it can see the `AbiTy` variants.
    for f in [
      RuntimeFn::ThreadInit,
      RuntimeFn::ThreadDeinit,
      RuntimeFn::ThreadRegister,
      RuntimeFn::ThreadUnregister,
      RuntimeFn::ThreadSetParked,
      RuntimeFn::RegisterShapeTable,
      RuntimeFn::Alloc,
      RuntimeFn::AllocPinned,
      RuntimeFn::AllocArray,
      RuntimeFn::GlobalRootRegister,
      RuntimeFn::GlobalRootUnregister,
      RuntimeFn::GcSafepoint,
      RuntimeFn::GcSafepointSlow,
      RuntimeFn::GcSafepointRelocateH,
      RuntimeFn::GcCollect,
      RuntimeFn::GcPoll,
      RuntimeFn::WriteBarrier,
      RuntimeFn::WriteBarrierRange,
      RuntimeFn::KeepAliveGcRef,
      RuntimeFn::ParallelSpawn,
      RuntimeFn::ParallelJoin,
      RuntimeFn::ParallelFor,
    ] {
      let decl = f.decl();
      let spec = decl.spec;
      let abi = decl.abi;
 
      assert_eq!(
        spec.gc_ptr_args,
        count_codegen_params(abi, AbiTy::GcPtr),
        "runtime fn spec mismatch for {f:?}: gc_ptr_args must equal number of AbiTy::GcPtr params"
      );
      assert_eq!(
        spec.gc_handle_args,
        count_codegen_params(abi, AbiTy::GcHandle),
        "runtime fn spec mismatch for {f:?}: gc_handle_args must equal number of AbiTy::GcHandle params"
      );
    }
  }
}
