pub use runtime_native_abi::{
  Coroutine, CoroutineId, CoroutineRef, HandleId, InternedId, IoWatcherId, LegacyPromiseRef, Microtask,
  PromiseLayout, PromiseRef, PromiseResolveInput, PromiseResolveKind, PromiseResolvePayload, RtCoroResumeFn,
  RtCoroStatus, RtCoroutineHeader, RtFd, RtGcConfig, RtGcLimits, RtGcStatsSnapshot, RtParallelForBodyFn,
  RtShapeDescriptor, RtShapeId, RtThreadKind, RtTaskFn, StringRef, TaskId, ThenableRef, ThenableVTable,
  TimerId, ValueRef, RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE, RT_PROMISE_RESOLVE_PROMISE,
  RT_PROMISE_RESOLVE_THENABLE, RT_PROMISE_RESOLVE_VALUE,
};

/// Callback passed to a typed thenable's `then` implementation.
///
/// This corresponds to the `resolve` function in the spec's `PromiseResolveThenableJob`.
pub type ThenableResolveCallback = extern "C" fn(*mut u8, PromiseResolveInput);

/// Callback passed to a typed thenable's `then` implementation.
///
/// This corresponds to the `reject` function in the spec's `PromiseResolveThenableJob`.
pub type ThenableRejectCallback = extern "C" fn(*mut u8, ValueRef);

const fn max_usize(a: usize, b: usize) -> usize {
  if a > b { a } else { b }
}

const fn round_up(value: usize, align: usize) -> usize {
  (value + align - 1) / align * align
}

const _: () = {
  use core::mem::{align_of, size_of};

  const PTR_SIZE: usize = size_of::<*const u8>();
  const PTR_ALIGN: usize = align_of::<*const u8>();

  // `StringRef` is `{ const uint8_t* ptr; size_t len; }` in the C header.
  const USIZE_SIZE: usize = size_of::<usize>();
  const USIZE_ALIGN: usize = align_of::<usize>();
  const STRINGREF_ALIGN: usize = max_usize(PTR_ALIGN, USIZE_ALIGN);
  const STRINGREF_OFF_LEN: usize = round_up(PTR_SIZE, USIZE_ALIGN);
  const STRINGREF_SIZE: usize = round_up(STRINGREF_OFF_LEN + USIZE_SIZE, STRINGREF_ALIGN);

  if size_of::<StringRef>() != STRINGREF_SIZE {
    panic!("StringRef ABI size mismatch");
  }
  if align_of::<StringRef>() != STRINGREF_ALIGN {
    panic!("StringRef ABI alignment mismatch");
  }

  // `RtCoroutineHeader` layout is part of the compiler/runtime ABI contract.
  const RESUME_SIZE: usize = size_of::<RtCoroResumeFn>();
  const RESUME_ALIGN: usize = align_of::<RtCoroResumeFn>();
  const PROMISE_SIZE: usize = size_of::<LegacyPromiseRef>();
  const PROMISE_ALIGN: usize = align_of::<LegacyPromiseRef>();
  const U32_SIZE: usize = size_of::<u32>();
  const U32_ALIGN: usize = align_of::<u32>();
  const VALUE_SIZE: usize = size_of::<ValueRef>();
  const VALUE_ALIGN: usize = align_of::<ValueRef>();

  const HEADER_ALIGN: usize = max_usize(
    max_usize(RESUME_ALIGN, PROMISE_ALIGN),
    max_usize(U32_ALIGN, VALUE_ALIGN),
  );

  const HEADER_OFF_PROMISE: usize = round_up(RESUME_SIZE, PROMISE_ALIGN);
  const HEADER_OFF_STATE: usize = round_up(HEADER_OFF_PROMISE + PROMISE_SIZE, U32_ALIGN);
  const HEADER_OFF_AWAIT_IS_ERROR: usize = round_up(HEADER_OFF_STATE + U32_SIZE, U32_ALIGN);
  const HEADER_OFF_AWAIT_VALUE: usize = round_up(HEADER_OFF_AWAIT_IS_ERROR + U32_SIZE, VALUE_ALIGN);
  const HEADER_OFF_AWAIT_ERROR: usize = round_up(HEADER_OFF_AWAIT_VALUE + VALUE_SIZE, VALUE_ALIGN);
  const HEADER_SIZE: usize = round_up(HEADER_OFF_AWAIT_ERROR + VALUE_SIZE, HEADER_ALIGN);

  if size_of::<RtCoroutineHeader>() != HEADER_SIZE {
    panic!("RtCoroutineHeader ABI size mismatch");
  }
  if align_of::<RtCoroutineHeader>() != HEADER_ALIGN {
    panic!("RtCoroutineHeader ABI alignment mismatch");
  }
};
