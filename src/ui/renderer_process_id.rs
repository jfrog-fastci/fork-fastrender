use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_RENDERER_PROCESS_ID: AtomicU64 = AtomicU64::new(1);

/// Identifier for a renderer process.
///
/// A `RendererProcessId` identifies a *renderer process* independently of any particular tab.
/// Today, tabs are typically mapped 1:1 to renderer processes, but this separation is foundational
/// for later process-sharing policies (e.g. per-origin) and process swaps.
///
/// Lifecycle expectations:
/// - **Ephemeral / process-local**: allocated monotonically within a single running browser
///   instance.
/// - **Not persisted**: values are not stable across restarts and must not be written into session
///   files.
///
/// `0` is reserved as an invalid value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RendererProcessId(pub u64);

impl RendererProcessId {
  /// Generate a new process-unique renderer process id.
  ///
  /// `0` is reserved as an invalid value, so the counter starts at 1. In the astronomically
  /// unlikely event that we wrap around `u64::MAX` (requiring ~1.8e19 allocations in a single
  /// process), skip over 0 and keep going rather than panicking.
  pub fn new() -> Self {
    loop {
      // `fetch_add` returns the previous value.
      let id = NEXT_RENDERER_PROCESS_ID.fetch_add(1, Ordering::Relaxed);
      if id != 0 {
        return Self(id);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  #[test]
  fn renderer_process_id_new_never_returns_zero() {
    for _ in 0..1024 {
      assert_ne!(RendererProcessId::new().0, 0);
    }
  }

  #[test]
  fn renderer_process_id_new_allocations_are_unique() {
    const N: usize = 10_000;
    let mut seen = HashSet::with_capacity(N);
    for _ in 0..N {
      let id = RendererProcessId::new().0;
      assert!(seen.insert(id), "duplicate RendererProcessId {id}");
    }
  }
}

