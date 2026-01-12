use core::ptr::null_mut;

use runtime_native::abi::{
  LegacyPromiseRef, PromiseResolveInput, PromiseResolveKind, PromiseResolvePayload, RtCoroStatus,
  RtCoroutineHeader, ValueRef,
};
use runtime_native::roots::GcHandle;

extern "C" fn dummy_resume(_coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  RtCoroStatus::Done
}

#[test]
fn legacy_exports_use_legacy_promise_ref_in_signatures() {
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

  let _promise_then: extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then;
  let _promise_then_legacy: extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), *mut u8) =
    runtime_native::rt_promise_then_legacy;
  let _promise_then_rooted_h: unsafe extern "C" fn(LegacyPromiseRef, extern "C" fn(*mut u8), GcHandle) =
    runtime_native::rt_promise_then_rooted_h;

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

  let _coro_await: extern "C" fn(*mut RtCoroutineHeader, LegacyPromiseRef, u32) = runtime_native::rt_coro_await;
  let _coro_await_legacy: extern "C" fn(*mut RtCoroutineHeader, LegacyPromiseRef, u32) =
    runtime_native::rt_coro_await_legacy;
}

#[test]
fn promise_resolve_input_payload_uses_legacy_promise_ref() {
  let p: LegacyPromiseRef = null_mut();
  let input = PromiseResolveInput::promise(p);
  assert_eq!(input.kind, PromiseResolveKind::Promise);

  let payload = PromiseResolvePayload { promise: p };
  let p2 = unsafe { payload.promise };
  assert_eq!(p2, p);
}

#[test]
fn rt_coroutine_header_promise_field_uses_legacy_promise_ref() {
  let _hdr = RtCoroutineHeader {
    resume: dummy_resume,
    promise: null_mut(),
    state: 0,
    await_is_error: 0,
    await_value: null_mut(),
    await_error: null_mut(),
  };
}
