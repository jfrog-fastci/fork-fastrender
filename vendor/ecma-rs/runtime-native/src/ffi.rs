use std::io::Write;

#[inline]
pub(crate) fn abort_on_panic<T>(f: impl FnOnce() -> T) -> T {
  #[cfg(panic = "unwind")]
  {
    // Some runtime entrypoints run inside `catch_unwind` to avoid unwinding across
    // the stable C ABI boundary.
    //
    // The sysroot's `catch_unwind` implementation is not guaranteed to maintain a
    // valid frame-pointer chain (it may repurpose the frame-pointer register),
    // which can break "walk outward to the nearest managed stackmap callsite"
    // logic in the safepoint slow path.
    //
    // Provide the current frame pointer as a best-effort override starting point
    // for safepoint-context fixups while `f` is executing.
    let fp = crate::stackwalk::current_frame_pointer();
    crate::threading::safepoint::with_safepoint_fixup_start_fp(fp, || {
      match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => std::process::abort(),
      }
    })
  }

  #[cfg(not(panic = "unwind"))]
  {
    f()
  }
}

#[cold]
#[inline(never)]
fn abort_due_to_panic_in_callback() -> ! {
  // Use best-effort direct stderr writes so we don't panic while reporting a
  // panic (e.g. if stderr is closed / broken pipe).
  let _ = std::io::stderr().write_all(b"runtime-native: panic in callback\n");
  std::process::abort();
}

#[inline]
pub(crate) fn abort_on_callback_panic<T>(f: impl FnOnce() -> T) -> T {
  #[cfg(panic = "unwind")]
  {
    // See `abort_on_panic` for the rationale for setting the safepoint fixup FP
    // override while running inside `catch_unwind`.
    let fp = crate::stackwalk::current_frame_pointer();
    crate::threading::safepoint::with_safepoint_fixup_start_fp(fp, || {
      match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => abort_due_to_panic_in_callback(),
      }
    })
  }

  #[cfg(not(panic = "unwind"))]
  {
    f()
  }
}

#[inline]
pub(crate) fn invoke_cb1(cb: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(*mut u8) = std::mem::transmute(cb);
    cb(data);
  });
}

#[inline]
pub(crate) fn invoke_cb2_u32(cb: extern "C" fn(u32, *mut u8), a0: u32, data: *mut u8) {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(u32, *mut u8) = std::mem::transmute(cb);
    cb(a0, data);
  });
}

#[inline]
pub(crate) fn invoke_cb2_usize(cb: extern "C" fn(usize, *mut u8), a0: usize, data: *mut u8) {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(usize, *mut u8) = std::mem::transmute(cb);
    cb(a0, data);
  });
}

#[inline]
pub(crate) fn invoke_cb2_promise(
  cb: extern "C" fn(*mut u8, crate::abi::PromiseRef),
  data: *mut u8,
  promise: crate::abi::PromiseRef,
) {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(*mut u8, crate::abi::PromiseRef) = std::mem::transmute(cb);
    cb(data, promise);
  });
}

#[inline]
pub(crate) fn invoke_cb2_legacy_promise(
  cb: extern "C" fn(*mut u8, crate::abi::LegacyPromiseRef),
  data: *mut u8,
  promise: crate::abi::LegacyPromiseRef,
) {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(*mut u8, crate::abi::LegacyPromiseRef) = std::mem::transmute(cb);
    cb(data, promise);
  });
}

#[inline]
pub(crate) fn invoke_cb2_blocking_promise_task(
  cb: extern "C" fn(*mut u8, *mut u8) -> u8,
  data: *mut u8,
  out_payload: *mut u8,
) -> u8 {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(*mut u8, *mut u8) -> u8 = std::mem::transmute(cb);
    cb(data, out_payload)
  })
}

#[inline]
pub(crate) fn invoke_coro_resume(
  cb: extern "C" fn(*mut crate::abi::RtCoroutineHeader) -> crate::abi::RtCoroStatus,
  coro: *mut crate::abi::RtCoroutineHeader,
) -> crate::abi::RtCoroStatus {
  abort_on_callback_panic(|| unsafe {
    let cb: extern "C-unwind" fn(*mut crate::abi::RtCoroutineHeader) -> crate::abi::RtCoroStatus =
      std::mem::transmute(cb);
    cb(coro)
  })
}

#[inline]
pub(crate) unsafe fn invoke_thenable_call(
  cb: unsafe extern "C" fn(
    thenable: *mut u8,
    on_fulfilled: crate::abi::ThenableResolveCallback,
    on_rejected: crate::abi::ThenableRejectCallback,
    data: *mut u8,
  ) -> crate::abi::ValueRef,
  thenable: *mut u8,
  on_fulfilled: crate::abi::ThenableResolveCallback,
  on_rejected: crate::abi::ThenableRejectCallback,
  data: *mut u8,
) -> crate::abi::ValueRef {
  abort_on_callback_panic(|| unsafe {
    let cb: unsafe extern "C-unwind" fn(
      thenable: *mut u8,
      on_fulfilled: crate::abi::ThenableResolveCallback,
      on_rejected: crate::abi::ThenableRejectCallback,
      data: *mut u8,
    ) -> crate::abi::ValueRef = std::mem::transmute(cb);
    cb(thenable, on_fulfilled, on_rejected, data)
  })
}
