//! Helpers for resetting thread-local paint scratch state.
//!
//! The paint pipeline keeps several per-thread scratch buffers (pixmaps/masks/Vecs) to avoid
//! repeated allocations while rendering. Unit tests can render multiple fixtures on the same
//! thread, so they need a way to clear these caches to ensure deterministic output and to support
//! "repeat render" tests.

/// Clears all thread-local scratch buffers used by paint/filter code on the current thread.
///
/// This is intentionally intended for test/harness usage: production code should not need to flush
/// scratch buffers to be deterministic, but tests/tools may want a clean slate to prove that output
/// is independent of prior renders.
pub fn reset_thread_local_scratch() {
  super::display_list_renderer::reset_thread_local_scratch_for_tests();
  super::blur::reset_thread_local_scratch_for_tests();
  super::painter::reset_thread_local_scratch_for_tests();
}

/// Best-effort reset of paint scratch buffers across the threads that may execute paint work.
///
/// This resets:
/// - the calling thread
/// - the global Rayon pool
/// - the dedicated paint pool (when `FASTR_PAINT_THREADS > 1` is set)
///
/// Tooling (such as `render_fixtures --reset-paint-scratch`) uses this to bisect paint
/// nondeterminism suspected to be related to scheduling-dependent scratch reuse.
pub fn reset_paint_scratch_best_effort() {
  reset_thread_local_scratch();

  // Reset on the global pool (or current pool, if called inside a custom install).
  rayon::broadcast(|_| {
    reset_thread_local_scratch();
  });

  // Reset on the dedicated paint pool if it is enabled. This is separate from the global pool.
  let paint_pool = super::paint_thread_pool::paint_pool();
  if let Some(pool) = paint_pool.pool {
    pool.install(|| {
      rayon::broadcast(|_| {
        reset_thread_local_scratch();
      });
    });
  }
}
