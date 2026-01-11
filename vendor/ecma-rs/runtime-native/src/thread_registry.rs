use crate::threading;
use crate::threading::ThreadKind;

/// Register the current OS thread as a GC mutator.
///
/// This is a lightweight convenience wrapper around the more explicit
/// `rt_thread_init(kind)` C ABI. Threads registered via this API are treated as
/// `Worker` threads.
#[no_mangle]
pub extern "C" fn rt_register_current_thread() {
  threading::register_current_thread(ThreadKind::Worker);
}

/// Unregister the current OS thread from the GC mutator set.
#[no_mangle]
pub extern "C" fn rt_unregister_current_thread() {
  threading::unregister_current_thread();
}

