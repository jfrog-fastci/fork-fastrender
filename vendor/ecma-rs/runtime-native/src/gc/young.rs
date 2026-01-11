use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// Atomic representation of the active nursery (young generation) address range.
///
/// The write barrier (`rt_write_barrier`) uses this range check to implement a fast `is_young(ptr)`
/// predicate.
///
/// # Updates
/// The GC must call `rt_gc_set_young_range`:
/// - during initialization (before any mutator stores that can hit the barrier), and
/// - after each nursery flip/resize that changes the active young-space range.
pub struct YoungSpace {
  pub start: AtomicUsize,
  pub end: AtomicUsize,
}

impl YoungSpace {
  pub const fn new() -> Self {
    Self {
      start: AtomicUsize::new(0),
      end: AtomicUsize::new(0),
    }
  }

  #[inline]
  pub fn contains(&self, ptr: usize) -> bool {
    let start = self.start.load(Ordering::Acquire);
    let end = self.end.load(Ordering::Acquire);
    ptr >= start && ptr < end
  }
}

pub(crate) static YOUNG_SPACE: YoungSpace = YoungSpace::new();

