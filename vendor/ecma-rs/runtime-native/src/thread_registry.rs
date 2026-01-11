use crate::threading;
use crate::threading::ThreadKind;

/// Register the current OS thread as a GC mutator.
///
/// This is a lightweight convenience wrapper around the more explicit
/// `rt_thread_init(kind)` C ABI. Threads registered via this API are treated as
/// `Worker` threads.
#[no_mangle]
pub extern "C" fn rt_register_current_thread() {
  crate::ffi::abort_on_panic(|| {
    threading::register_current_thread(ThreadKind::Worker);
  });
}

/// Register the current OS thread as a GC mutator.
///
/// Compatibility alias for earlier codegen prototypes that expect a `rt_register_thread` symbol.
/// Newer code should prefer [`rt_thread_init`](crate::rt_thread_init) or
/// [`rt_register_current_thread`] so the runtime can track thread kinds.
#[no_mangle]
pub extern "C" fn rt_register_thread() {
  crate::ffi::abort_on_panic(|| {
    rt_register_current_thread();
  });
}

/// Unregister the current OS thread from the GC mutator set.
#[no_mangle]
pub extern "C" fn rt_unregister_current_thread() {
  crate::ffi::abort_on_panic(|| {
    threading::unregister_current_thread();
  });
}

/// Unregister the current OS thread from the GC mutator set.
///
/// Compatibility alias for earlier codegen prototypes that expect a `rt_unregister_thread` symbol.
#[no_mangle]
pub extern "C" fn rt_unregister_thread() {
  crate::ffi::abort_on_panic(|| {
    rt_unregister_current_thread();
  });
}
