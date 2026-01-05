//! Test-only helpers for resetting thread-local paint scratch state.
//!
//! The paint pipeline keeps several per-thread scratch buffers (pixmaps/masks/Vecs) to avoid
//! repeated allocations while rendering. Unit tests can render multiple fixtures on the same
//! thread, so they need a way to clear these caches to ensure deterministic output and to support
//! "repeat render" tests.

/// Clears all thread-local scratch buffers used by paint/filter code on the current thread.
///
/// This is intentionally test-only: production code should not need to flush scratch buffers to be
/// deterministic, but tests may want a clean slate to prove that output is independent of prior
/// renders.
pub fn reset_thread_local_scratch() {
  super::display_list_renderer::reset_thread_local_scratch_for_tests();
  super::blur::reset_thread_local_scratch_for_tests();
  super::painter::reset_thread_local_scratch_for_tests();
}

