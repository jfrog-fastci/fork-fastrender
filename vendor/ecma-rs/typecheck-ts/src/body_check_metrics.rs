use std::sync::atomic::{AtomicUsize, Ordering};

/// Global instrumentation hook for counting actual body-check executions.
///
/// Parsing/lowering counters are thread-local because they only run on the
/// calling thread, but body checking can be parallelized via rayon. Use a global
/// atomic so integration tests can reliably observe recomputation regardless of
/// scheduling.
static BODY_CHECK_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Number of times body checking has been performed since the last reset.
///
/// This counter is incremented at the start of the body checker implementation
/// (before caching within a single `BodyCheckDb` instance), so cached reads do
/// not affect the count—only real recomputation does.
pub fn body_check_call_count() -> usize {
  BODY_CHECK_CALLS.load(Ordering::Relaxed)
}

/// Reset the body-check invocation counter to zero.
pub fn reset_body_check_call_count() {
  BODY_CHECK_CALLS.store(0, Ordering::Relaxed);
}

pub(crate) fn record_body_check_call() {
  BODY_CHECK_CALLS.fetch_add(1, Ordering::Relaxed);
}

