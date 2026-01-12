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
  /// Append additional shapes to the global shape table and return the first assigned id.
  ///
  /// `rt_register_shape_table_extend(ptr: *const RtShapeDescriptor, len: usize) -> RtShapeId`
  RegisterShapeTableExtend,
  /// Compatibility alias for [`RuntimeFn::RegisterShapeTableExtend`].
  ///
  /// `rt_register_shape_table_append(ptr: *const RtShapeDescriptor, len: usize) -> RtShapeId`
  RegisterShapeTableAppend,
  /// Register a single shape descriptor and return its assigned id.
  ///
  /// `rt_register_shape(desc: *const RtShapeDescriptor) -> RtShapeId`
  RegisterShape,
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
  // Interned strings
  // -----------------------------------------------------------------------------
  /// Intern a UTF-8 byte string and return a stable `InternedId` (`u32`).
  ///
  /// `rt_string_intern(const uint8_t* s, size_t len) -> InternedId`
  StringIntern,
  /// Permanently pin an interned string so it survives GC sweeps and interner pruning.
  ///
  /// `rt_string_pin_interned(InternedId id) -> void`
  StringPinInterned,

  // -----------------------------------------------------------------------------
  // Strings
  // -----------------------------------------------------------------------------
  /// Allocate a GC-managed UTF-8 string and copy `bytes`.
  ///
  /// `rt_string_new_utf8(bytes: *const u8, len: usize) -> GcPtr`
  StringNewUtf8,
  /// Return the length (in UTF-8 bytes) of a GC-managed string.
  ///
  /// `rt_string_len(s: GcPtr) -> usize`
  StringLen,

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

  // -----------------------------------------------------------------------------
  // Persistent handles (stable u64 ids)
  // -----------------------------------------------------------------------------
  /// `rt_handle_alloc(ptr: *mut u8) -> HandleId`
  HandleAlloc,
  /// `rt_handle_alloc_h(ptr: GcHandle) -> HandleId`
  HandleAllocH,
  /// `rt_handle_free(handle: HandleId)`
  HandleFree,
  /// `rt_handle_load(handle: HandleId) -> GcPtr`
  HandleLoad,
  /// `rt_handle_store(handle: HandleId, ptr: *mut u8)`
  HandleStore,
  /// `rt_handle_store_h(handle: HandleId, ptr: GcHandle)`
  HandleStoreH,

  // -----------------------------------------------------------------------------
  // Weak handles (non-owning references)
  // -----------------------------------------------------------------------------
  /// `rt_weak_add(value: *mut u8) -> u64`
  WeakAdd,
  /// `rt_weak_add_h(value: GcHandle) -> u64`
  WeakAddH,
  /// `rt_weak_get(handle: u64) -> GcPtr`
  WeakGet,
  /// `rt_weak_remove(handle: u64)`
  WeakRemove,

  // -----------------------------------------------------------------------------
  // Native async runtime ABI v2
  // -----------------------------------------------------------------------------
  /// `rt_async_spawn(coro: CoroutineId) -> PromiseRef`
  AsyncSpawn,
  /// `rt_async_spawn_deferred(coro: CoroutineId) -> PromiseRef`
  AsyncSpawnDeferred,
  /// `rt_async_cancel_all()`
  AsyncCancelAll,
  /// `rt_async_poll() -> bool`
  AsyncPoll,
  /// `rt_async_wait()`
  AsyncWait,
  /// `rt_async_set_strict_await_yields(strict: bool)`
  AsyncSetStrictAwaitYields,
  /// `rt_async_run_until_idle() -> bool`
  AsyncRunUntilIdle,
  /// `rt_async_block_on(p: PromiseRef)`
  AsyncBlockOn,
  /// `rt_async_sleep(delay_ms: u64) -> PromiseRef`
  AsyncSleep,
  /// `rt_drain_microtasks() -> bool`
  DrainMicrotasks,

  // -----------------------------------------------------------------------------
  // Native promise ABI v2 (PromiseHeader prefix)
  // -----------------------------------------------------------------------------
  /// `rt_promise_init(p: PromiseRef)`
  PromiseInit,
  /// `rt_promise_fulfill(p: PromiseRef)`
  PromiseFulfill,
  /// `rt_promise_try_fulfill(p: PromiseRef) -> bool`
  PromiseTryFulfill,
  /// `rt_promise_reject(p: PromiseRef)`
  PromiseReject,
  /// `rt_promise_try_reject(p: PromiseRef) -> bool`
  PromiseTryReject,
  /// `rt_promise_mark_handled(p: PromiseRef)`
  PromiseMarkHandled,
  /// `rt_promise_payload_ptr(p: PromiseRef) -> *mut u8`
  PromisePayloadPtr,

  // -----------------------------------------------------------------------------
  // Rooted scheduling APIs (HandleId-based)
  // -----------------------------------------------------------------------------
  /// `rt_queue_microtask_handle(cb: extern "C" fn(*mut u8), data: HandleId)`
  QueueMicrotaskHandle,
  /// `rt_queue_microtask_handle_with_drop(cb: extern "C" fn(*mut u8), data: HandleId, drop_data: extern "C" fn(*mut u8))`
  QueueMicrotaskHandleWithDrop,
  /// `rt_set_timeout_handle(cb: extern "C" fn(*mut u8), data: HandleId, delay_ms: u64) -> TimerId`
  SetTimeoutHandle,
  /// `rt_set_timeout_handle_with_drop(cb: extern "C" fn(*mut u8), data: HandleId, drop_data: extern "C" fn(*mut u8), delay_ms: u64) -> TimerId`
  SetTimeoutHandleWithDrop,
  /// `rt_set_interval_handle(cb: extern "C" fn(*mut u8), data: HandleId, interval_ms: u64) -> TimerId`
  SetIntervalHandle,
  /// `rt_set_interval_handle_with_drop(cb: extern "C" fn(*mut u8), data: HandleId, drop_data: extern "C" fn(*mut u8), interval_ms: u64) -> TimerId`
  SetIntervalHandleWithDrop,
  /// `rt_io_register_handle(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: HandleId) -> IoWatcherId`
  IoRegisterHandle,
  /// `rt_io_register_handle_with_drop(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: HandleId, drop_data: extern "C" fn(*mut u8)) -> IoWatcherId`
  IoRegisterHandleWithDrop,
  /// `rt_io_update(id: IoWatcherId, interests: u32)`
  IoUpdate,
  /// `rt_io_unregister(id: IoWatcherId)`
  IoUnregister,
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
      RuntimeFn::RegisterShapeTableExtend => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_register_shape_table_extend",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I32,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I32,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::RegisterShapeTableAppend => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_register_shape_table_append",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I32,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I32,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::RegisterShape => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_register_shape",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I32,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I32,
          codegen_params: &[AbiTy::RawPtr],
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
      RuntimeFn::StringIntern => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_string_intern",
          // Conservatively treat string interning as MayGC: it may allocate and it can block on
          // GC-aware locks, during which stop-the-world GC may occur.
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I32,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I32,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::StringPinInterned => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_string_pin_interned",
          // Conservatively treat pinning as MayGC: it may allocate/copy, and it can block on
          // GC-aware locks, during which stop-the-world GC may occur.
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
      RuntimeFn::StringNewUtf8 => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_string_new_utf8",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::StringLen => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_string_len",
          may_gc: false,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I64,
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

      // -----------------------------------------------------------------------------
      // Persistent handles
      // -----------------------------------------------------------------------------
      RuntimeFn::HandleAlloc => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_alloc",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr],
        },
      },
      RuntimeFn::HandleAllocH => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_alloc_h",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 1,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::GcHandle],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::GcHandle],
        },
      },
      RuntimeFn::HandleFree => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_free",
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
      RuntimeFn::HandleLoad => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_load",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::HandleStore => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_store",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I64, AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I64, AbiTy::RawPtr],
        },
      },
      RuntimeFn::HandleStoreH => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_handle_store_h",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 1,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I64, AbiTy::GcHandle],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I64, AbiTy::GcHandle],
        },
      },

      // -----------------------------------------------------------------------------
      // Weak handles
      // -----------------------------------------------------------------------------
      RuntimeFn::WeakAdd => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_weak_add",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr],
        },
      },
      RuntimeFn::WeakAddH => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_weak_add_h",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 1,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::GcHandle],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::GcHandle],
        },
      },
      RuntimeFn::WeakGet => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_weak_get",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::WeakRemove => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_weak_remove",
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

      // -----------------------------------------------------------------------------
      // Native async runtime ABI v2
      // -----------------------------------------------------------------------------
      RuntimeFn::AsyncSpawn => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_spawn",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::AsyncSpawnDeferred => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_spawn_deferred",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::AsyncCancelAll => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_cancel_all",
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
      RuntimeFn::AsyncPoll => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_poll",
          may_gc: true,
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
      RuntimeFn::AsyncWait => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_wait",
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
      RuntimeFn::AsyncSetStrictAwaitYields => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_set_strict_await_yields",
          may_gc: false,
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
      RuntimeFn::AsyncRunUntilIdle => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_run_until_idle",
          may_gc: true,
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
      RuntimeFn::AsyncBlockOn => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_block_on",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::AsyncSleep => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_async_sleep",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::I64],
          codegen_ret: AbiTy::GcPtr,
          codegen_params: &[AbiTy::I64],
        },
      },
      RuntimeFn::DrainMicrotasks => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_drain_microtasks",
          may_gc: true,
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

      // -----------------------------------------------------------------------------
      // Native promise ABI v2
      // -----------------------------------------------------------------------------
      RuntimeFn::PromiseInit => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_init",
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
      RuntimeFn::PromiseFulfill => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_fulfill",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::PromiseTryFulfill => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_try_fulfill",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I1,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I1,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::PromiseReject => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_reject",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::PromiseTryReject => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_try_reject",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I1,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::I1,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::PromiseMarkHandled => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_mark_handled",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::GcPtr],
        },
      },
      RuntimeFn::PromisePayloadPtr => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_promise_payload_ptr",
          may_gc: true,
          gc_ptr_args: 1,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::RawPtr,
          runtime_params: &[AbiTy::RawPtr],
          codegen_ret: AbiTy::RawPtr,
          codegen_params: &[AbiTy::GcPtr],
        },
      },

      // -----------------------------------------------------------------------------
      // Rooted scheduling APIs (HandleId-based)
      // -----------------------------------------------------------------------------
      RuntimeFn::QueueMicrotaskHandle => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_queue_microtask_handle",
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
      RuntimeFn::QueueMicrotaskHandleWithDrop => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_queue_microtask_handle_with_drop",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr],
        },
      },
      RuntimeFn::SetTimeoutHandle => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_set_timeout_handle",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::I64],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::I64],
        },
      },
      RuntimeFn::SetTimeoutHandleWithDrop => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_set_timeout_handle_with_drop",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::SetIntervalHandle => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_set_interval_handle",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::I64],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::I64],
        },
      },
      RuntimeFn::SetIntervalHandleWithDrop => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_set_interval_handle_with_drop",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::IoRegisterHandle => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_io_register_handle",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::I32, AbiTy::I32, AbiTy::RawPtr, AbiTy::I64],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::I32, AbiTy::I32, AbiTy::RawPtr, AbiTy::I64],
        },
      },
      RuntimeFn::IoRegisterHandleWithDrop => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_io_register_handle_with_drop",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::I64,
          runtime_params: &[AbiTy::I32, AbiTy::I32, AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr],
          codegen_ret: AbiTy::I64,
          codegen_params: &[AbiTy::I32, AbiTy::I32, AbiTy::RawPtr, AbiTy::I64, AbiTy::RawPtr],
        },
      },
      RuntimeFn::IoUpdate => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_io_update",
          may_gc: true,
          gc_ptr_args: 0,
          gc_handle_args: 0,
          arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
        },
        abi: RuntimeFnAbi {
          runtime_ret: AbiTy::Void,
          runtime_params: &[AbiTy::I64, AbiTy::I32],
          codegen_ret: AbiTy::Void,
          codegen_params: &[AbiTy::I64, AbiTy::I32],
        },
      },
      RuntimeFn::IoUnregister => RuntimeFnDecl {
        spec: RuntimeFnSpec {
          name: "rt_io_unregister",
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
      RuntimeFn::RegisterShapeTableExtend,
      RuntimeFn::RegisterShapeTableAppend,
      RuntimeFn::RegisterShape,
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
      RuntimeFn::StringIntern,
      RuntimeFn::StringPinInterned,
      RuntimeFn::ParallelSpawn,
      RuntimeFn::ParallelJoin,
      RuntimeFn::ParallelFor,
      RuntimeFn::HandleAlloc,
      RuntimeFn::HandleAllocH,
      RuntimeFn::HandleFree,
      RuntimeFn::HandleLoad,
      RuntimeFn::HandleStore,
      RuntimeFn::HandleStoreH,
      RuntimeFn::WeakAdd,
      RuntimeFn::WeakAddH,
      RuntimeFn::WeakGet,
      RuntimeFn::WeakRemove,
      RuntimeFn::AsyncSpawn,
      RuntimeFn::AsyncSpawnDeferred,
      RuntimeFn::AsyncCancelAll,
      RuntimeFn::AsyncPoll,
      RuntimeFn::AsyncWait,
      RuntimeFn::AsyncSetStrictAwaitYields,
      RuntimeFn::AsyncRunUntilIdle,
      RuntimeFn::AsyncBlockOn,
      RuntimeFn::AsyncSleep,
      RuntimeFn::DrainMicrotasks,
      RuntimeFn::PromiseInit,
      RuntimeFn::PromiseFulfill,
      RuntimeFn::PromiseTryFulfill,
      RuntimeFn::PromiseReject,
      RuntimeFn::PromiseTryReject,
      RuntimeFn::PromiseMarkHandled,
      RuntimeFn::PromisePayloadPtr,
      RuntimeFn::QueueMicrotaskHandle,
      RuntimeFn::QueueMicrotaskHandleWithDrop,
      RuntimeFn::SetTimeoutHandle,
      RuntimeFn::SetTimeoutHandleWithDrop,
      RuntimeFn::SetIntervalHandle,
      RuntimeFn::SetIntervalHandleWithDrop,
      RuntimeFn::IoRegisterHandle,
      RuntimeFn::IoRegisterHandleWithDrop,
      RuntimeFn::IoUpdate,
      RuntimeFn::IoUnregister,
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
