//! Process-global old-to-young remembered set used by the generational write barrier.
//!
//! The exported C ABI entrypoint `rt_write_barrier` is classified as `NoGC` and
//! must not allocate or safepoint. Still, we need a real remembered-set buffer
//! so stop-the-world minor GC can discover old→young edges created by mutator
//! stores.
//!
//! This module provides a simple fixed-capacity buffer:
//! - The barrier sets the per-object `REMEMBERED` bit idempotently and, on the
//!   0→1 transition, records the object base pointer in the global buffer.
//! - GC drains the buffer into a [`super::SimpleRememberedSet`].
//!
//! If the buffer overflows we abort: failing to record an old→young edge is
//! unsound for a generational collector.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::ObjHeader;
use super::SimpleRememberedSet;

/// Maximum number of distinct remembered objects that can be recorded between
/// drains.
///
/// ~1M entries = 8MB on 64-bit.
const REMEMBERED_SET_CAPACITY: usize = 1 << 20;

pub struct GlobalRememberedSet {
  len: AtomicUsize,
  entries: [AtomicUsize; REMEMBERED_SET_CAPACITY],
}

impl GlobalRememberedSet {
  const fn new() -> Self {
    Self {
      len: AtomicUsize::new(0),
      entries: [const { AtomicUsize::new(0) }; REMEMBERED_SET_CAPACITY],
    }
  }

  #[inline]
  fn insert(&self, obj: *mut u8) {
    debug_assert!(!obj.is_null());
    let idx = self.len.fetch_add(1, Ordering::AcqRel);
    if idx >= REMEMBERED_SET_CAPACITY {
      // The write barrier must not allocate, so we cannot grow. Overflow would
      // allow missing an old→young edge, which can lead to use-after-move/free
      // during minor GC.
      std::process::abort();
    }
    self.entries[idx].store(obj as usize, Ordering::Release);
  }

  /// Clear the raw-pointer tracking list.
  ///
  /// This does **not** clear the per-object `REMEMBERED` header bits; callers
  /// that need to reset bits should do so explicitly on the objects they own.
  fn clear(&self) {
    let len = self.len.swap(0, Ordering::AcqRel).min(REMEMBERED_SET_CAPACITY);
    for i in 0..len {
      self.entries[i].store(0, Ordering::Release);
    }
  }

  /// Drain the current buffer into `dst`.
  ///
  /// # Stop-the-world requirement
  /// This must only be called when no mutator threads can be concurrently
  /// executing the write barrier.
  fn drain_into(&self, dst: &mut SimpleRememberedSet) {
    let len = self.len.swap(0, Ordering::AcqRel).min(REMEMBERED_SET_CAPACITY);
    for i in 0..len {
      let obj = self.entries[i].swap(0, Ordering::AcqRel) as *mut u8;
      if obj.is_null() {
        continue;
      }

      // The write barrier already set the remembered bit. `SimpleRememberedSet`
      // only enqueues on the 0→1 transition, so clear first while the world is
      // stopped.
      //
      // SAFETY: entries originate from the write barrier contract (object base
      // pointers).
      unsafe {
        (&*(obj as *const ObjHeader)).clear_remembered_idempotent();
      }
      dst.remember(obj);
    }
  }
}

static GLOBAL_REMSET: GlobalRememberedSet = GlobalRememberedSet::new();

/// Global singleton remembered-set state used by the write barrier.
pub fn remembered_set() -> &'static GlobalRememberedSet {
  &GLOBAL_REMSET
}

/// Fast-path entry used by `rt_write_barrier`.
///
/// Sets the object's `REMEMBERED` header bit and, on the 0→1 transition,
/// records the object in the global remembered-set buffer.
#[inline]
pub fn remset_add(obj: *mut u8) {
  if obj.is_null() {
    return;
  }

  // SAFETY: The write barrier contract guarantees `obj` is an object base
  // pointer (start of `ObjHeader`).
  let header = unsafe { &*(obj as *const ObjHeader) };
  if header.set_remembered_idempotent() {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_remembered_object_added();
    remembered_set().insert(obj);
  }
}

/// Clear the process-global tracking list.
///
/// This is used by tests to avoid leaving dangling raw pointers behind.
pub(crate) fn remset_clear() {
  remembered_set().clear();
}

/// Returns the number of objects currently recorded in the global remembered set.
///
/// Intended for tests and debugging only.
pub(crate) fn remset_len_for_tests() -> usize {
  remembered_set()
    .len
    .load(Ordering::Acquire)
    .min(REMEMBERED_SET_CAPACITY)
}

/// Drain the process-global buffer into `dst`.
///
/// Intended to be called by the GC at the beginning of a minor collection.
pub fn remset_drain_into(dst: &mut SimpleRememberedSet) {
  remembered_set().drain_into(dst);
}
