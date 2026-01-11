use crate::threading::registry;
use crate::threading::registry::ThreadKind;

pub use crate::threading::registry::ThreadState;

/// Run `f` with the current thread's registered [`ThreadState`].
///
/// If the thread has not been registered yet, it is registered as
/// [`ThreadKind::External`].
pub fn with_thread_state<R>(f: impl FnOnce(&ThreadState) -> R) -> R {
  // Ensure the thread is registered so GC can enumerate its shadow stack during stop-the-world.
  if registry::current_thread_state().is_none() {
    registry::register_current_thread(ThreadKind::External);
  }

  let ts = registry::current_thread_state().expect("thread must be registered");
  f(&ts)
}
