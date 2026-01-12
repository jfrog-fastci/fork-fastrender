use runtime_native::abi::{LegacyPromiseRef, PromiseRef, PromiseResolveInput, RtCoroutineHeader, ThenableRef, ValueRef};
use runtime_native::roots::GcHandle;

/// Regression test: keep the legacy async-rt export signatures aligned with `runtime_native.h`.
///
/// Most legacy promise/coroutine APIs operate on `LegacyPromiseRef` handles, but the legacy-style
/// `rt_promise_then*_legacy` callbacks take `PromiseRef` so they can be used with native GC-managed
/// promises as well.
#[test]
fn legacy_export_signatures_match_header() {
  let _spawn_blocking: extern "C" fn(
    extern "C" fn(*mut u8, LegacyPromiseRef),
    *mut u8,
  ) -> LegacyPromiseRef = runtime_native::rt_spawn_blocking;

  let _parallel_spawn_promise_legacy: extern "C" fn(
    extern "C" fn(*mut u8, LegacyPromiseRef),
    *mut u8,
  ) -> LegacyPromiseRef = runtime_native::rt_parallel_spawn_promise_legacy;

  let _async_spawn_legacy: extern "C" fn(*mut RtCoroutineHeader) -> LegacyPromiseRef =
    runtime_native::rt_async_spawn_legacy;
  let _async_spawn_deferred_legacy: extern "C" fn(*mut RtCoroutineHeader) -> LegacyPromiseRef =
    runtime_native::rt_async_spawn_deferred_legacy;
  let _async_sleep_legacy: extern "C" fn(u64) -> LegacyPromiseRef = runtime_native::rt_async_sleep_legacy;

  let _coro_await_legacy: extern "C" fn(*mut RtCoroutineHeader, LegacyPromiseRef, u32) =
    runtime_native::rt_coro_await_legacy;
  let _coro_await: extern "C" fn(*mut RtCoroutineHeader, LegacyPromiseRef, u32) =
    runtime_native::rt_coro_await;

  let _promise_new: extern "C" fn() -> LegacyPromiseRef = runtime_native::rt_promise_new;
  let _promise_new_legacy: extern "C" fn() -> LegacyPromiseRef = runtime_native::rt_promise_new_legacy;

  let _promise_resolve: extern "C" fn(LegacyPromiseRef, ValueRef) = runtime_native::rt_promise_resolve;
  let _promise_resolve_legacy: extern "C" fn(LegacyPromiseRef, ValueRef) =
    runtime_native::rt_promise_resolve_legacy;
  let _promise_reject_legacy: extern "C" fn(LegacyPromiseRef, ValueRef) =
    runtime_native::rt_promise_reject_legacy;

  let _promise_resolve_into_legacy: extern "C" fn(LegacyPromiseRef, PromiseResolveInput) =
    runtime_native::rt_promise_resolve_into_legacy;
  let _promise_resolve_promise_legacy: extern "C" fn(LegacyPromiseRef, LegacyPromiseRef) =
    runtime_native::rt_promise_resolve_promise_legacy;
  let _promise_resolve_thenable_legacy: extern "C" fn(LegacyPromiseRef, ThenableRef) =
    runtime_native::rt_promise_resolve_thenable_legacy;

  let _promise_then: extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then;
  let _promise_then_legacy: extern "C" fn(PromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then_legacy;

  let _promise_then_rooted: extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then_rooted;
  let _promise_then_rooted_legacy: extern "C" fn(PromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then_rooted_legacy;

  let _promise_then_rooted_h: unsafe extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), GcHandle) =
    runtime_native::rt_promise_then_rooted_h;
  let _promise_then_rooted_h_legacy: unsafe extern "C" fn(
    PromiseRef,
    extern "C" fn(*mut u8),
    GcHandle,
  ) = runtime_native::rt_promise_then_rooted_h_legacy;

  let _promise_then_with_drop_legacy: extern "C" fn(
    PromiseRef,
    extern "C" fn(*mut u8),
    *mut u8,
    extern "C" fn(*mut u8),
  ) = runtime_native::rt_promise_then_with_drop_legacy;

  let _promise_drop_legacy: extern "C" fn(LegacyPromiseRef) = runtime_native::rt_promise_drop_legacy;
}
