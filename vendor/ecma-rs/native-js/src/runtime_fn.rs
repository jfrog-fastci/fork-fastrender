/// Metadata about runtime functions callable from compiled code.
///
/// This module is the single registry for runtime entrypoints (Task 315). It encodes the GC-safety
/// properties needed by codegen:
///
/// - Whether a call may trigger GC (`may_gc`).
/// - Whether the ABI contains raw GC pointers (`gc_ptr_args`), which is **unsound** for `may_gc`
///   runtime functions unless the runtime provides its own argument-rooting mechanism.
///
/// See `native-js/docs/llvm_gc_strategy.md` for the full rationale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeFn {
  /// Allocation entrypoint: always may trigger GC.
  Alloc,
  /// Pinned allocation entrypoint: always may trigger GC.
  AllocPinned,
  /// GC safepoint poll.
  GcSafepoint,
  /// Slow path for explicit safepoint polling:
  /// `rt_gc_safepoint_slow(epoch: u64)`.
  ///
  /// This is used by backedge polling fast paths (see `codegen::safepoint`).
  GcSafepointSlow,
  /// Forces a GC cycle.
  GcCollect,
  /// Poll-only helper (`rt_gc_poll() -> bool`).
  ///
  /// This must not allocate or safepoint.
  GcPoll,
  /// Generational write barrier (must not allocate / GC).
  WriteBarrier,
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
  pub arg_rooting: ArgRootingPolicy,
}

impl RuntimeFn {
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
    match self {
      RuntimeFn::Alloc => RuntimeFnSpec {
        name: "rt_alloc",
        may_gc: true,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::AllocPinned => RuntimeFnSpec {
        name: "rt_alloc_pinned",
        may_gc: true,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::GcSafepoint => RuntimeFnSpec {
        name: "rt_gc_safepoint",
        may_gc: true,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::GcSafepointSlow => RuntimeFnSpec {
        name: "rt_gc_safepoint_slow",
        may_gc: true,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::GcCollect => RuntimeFnSpec {
        name: "rt_gc_collect",
        may_gc: true,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::GcPoll => RuntimeFnSpec {
        name: "rt_gc_poll",
        may_gc: false,
        gc_ptr_args: 0,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::WriteBarrier => RuntimeFnSpec {
        name: "rt_write_barrier",
        may_gc: false,
        gc_ptr_args: 2,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
      RuntimeFn::KeepAliveGcRef => RuntimeFnSpec {
        name: "rt_keep_alive_gc_ref",
        may_gc: false,
        gc_ptr_args: 1,
        arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
      },
    }
  }
}
