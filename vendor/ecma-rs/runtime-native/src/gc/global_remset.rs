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

use core::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::ObjHeader;
use super::RememberedSet;
use super::SimpleRememberedSet;
use crate::threading::registry;

/// Maximum number of distinct remembered objects that can be recorded between
/// drains.
///
/// ~1M entries = 8MB on 64-bit.
const REMEMBERED_SET_CAPACITY: usize = 1 << 20;

/// Maximum number of remembered objects recorded per registered thread between
/// drains.
///
/// This is intentionally much smaller than the process-global buffer: entries
/// can be flushed into the global buffer if a thread overflows its local quota.
///
/// 16k entries = 128KB on 64-bit.
const THREAD_REMSET_CAPACITY: usize = 1 << 14;

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
      // Entries originate from the write barrier contract (object base pointers)
      // and already have their `REMEMBERED` bit set. When draining into
      // `SimpleRememberedSet`, we must enqueue even if the bit is already set,
      // and we must not double-count stats.
      dst.remember_from_write_barrier_buffer(obj);
    }
  }
}

static GLOBAL_REMSET: GlobalRememberedSet = GlobalRememberedSet::new();

/// Per-thread remembered-set buffer stored in the thread registry.
///
/// This keeps the `rt_write_barrier` hot path allocation-free and avoids a
/// contended global atomic on every remembered insert.
///
/// If a thread buffer overflows, entries fall back to the process-global buffer
/// (which is also allocation-free).
pub(crate) struct ThreadRemsetBuffer {
  len: AtomicUsize,
  entries: [AtomicUsize; THREAD_REMSET_CAPACITY],
}

impl fmt::Debug for ThreadRemsetBuffer {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ThreadRemsetBuffer")
      .field("len", &self.len.load(Ordering::Acquire))
      .finish()
  }
}

impl ThreadRemsetBuffer {
  pub(crate) const fn new() -> Self {
    Self {
      len: AtomicUsize::new(0),
      entries: [const { AtomicUsize::new(0) }; THREAD_REMSET_CAPACITY],
    }
  }

  /// Iterate all recorded object pointers without modifying the buffer.
  ///
  /// # Stop-the-world requirement
  /// This must only be called when no mutator threads can be concurrently
  /// executing the write barrier on this thread state.
  pub(crate) fn for_each_raw(&self, mut f: impl FnMut(*mut u8)) {
    let len = self.len.load(Ordering::Acquire).min(THREAD_REMSET_CAPACITY);
    for i in 0..len {
      let obj = self.entries[i].load(Ordering::Acquire) as *mut u8;
      if obj.is_null() {
        continue;
      }
      f(obj);
    }
  }

  #[inline]
  pub(crate) fn insert(&self, obj: *mut u8) {
    debug_assert!(!obj.is_null());
    let idx = self.len.fetch_add(1, Ordering::AcqRel);
    if idx >= THREAD_REMSET_CAPACITY {
      // Saturate so future inserts fast-path to the global buffer without the
      // counter growing unbounded.
      self.len.store(THREAD_REMSET_CAPACITY, Ordering::Release);
      remembered_set().insert(obj);
      return;
    }
    self.entries[idx].store(obj as usize, Ordering::Release);
  }

  /// Drain all recorded object pointers, resetting the buffer to empty.
  ///
  /// # Stop-the-world requirement
  /// This must only be called when no mutator threads can be concurrently
  /// executing the write barrier on this thread state.
  pub(crate) fn drain_raw(&self, mut f: impl FnMut(*mut u8)) {
    let len = self.len.swap(0, Ordering::AcqRel).min(THREAD_REMSET_CAPACITY);
    for i in 0..len {
      let obj = self.entries[i].swap(0, Ordering::AcqRel) as *mut u8;
      if obj.is_null() {
        continue;
      }
      f(obj);
    }
  }

  /// Clear the raw-pointer tracking list.
  ///
  /// This does **not** clear the per-object `REMEMBERED` header bits; callers
  /// that need to reset bits should do so explicitly on the objects they own.
  pub(crate) fn clear(&self) {
    let len = self.len.swap(0, Ordering::AcqRel).min(THREAD_REMSET_CAPACITY);
    for i in 0..len {
      self.entries[i].store(0, Ordering::Release);
    }
  }

  pub(crate) fn len_for_tests(&self) -> usize {
    self.len.load(Ordering::Acquire).min(THREAD_REMSET_CAPACITY)
  }
}

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
    let thread = registry::current_thread_state_ptr();
    if !thread.is_null() {
      // SAFETY: `current_thread_state_ptr` is set during thread registration and
      // cleared before unregistering/dropping the TLS `ThreadRegistration`.
      unsafe {
        (&*thread).remset_record(obj);
      }
    } else {
      remembered_set().insert(obj);
    }
  }
}

/// Clear the process-global tracking list.
///
/// This is used by tests to avoid leaving dangling raw pointers behind.
pub(crate) fn remset_clear() {
  remembered_set().clear();
  registry::for_each_thread(|thread| thread.remset_clear_for_tests());
}

/// Returns the number of objects currently recorded in the global remembered set.
///
/// Intended for tests and debugging only.
pub(crate) fn remset_len_for_tests() -> usize {
  let mut total = remembered_set()
    .len
    .load(Ordering::Acquire)
    .min(REMEMBERED_SET_CAPACITY);
  registry::for_each_thread(|thread| {
    total = total.saturating_add(thread.remset_len_for_tests());
  });
  total
}

/// Flush a thread's remembered-set buffer into the global buffer.
///
/// This is used when a thread unregisters/exits: we must not lose old→young
/// edges recorded by that thread, even if the `ThreadState` is dropped.
pub(crate) fn remset_flush_thread_to_global(thread: &registry::ThreadState) {
  thread.remset_drain_raw(|obj| remembered_set().insert(obj));
}

/// Drain the process-global buffer into `dst`.
///
/// Intended to be called by the GC at the beginning of a minor collection.
pub fn remset_drain_into(dst: &mut SimpleRememberedSet) {
  remembered_set().drain_into(dst);
  registry::for_each_thread(|thread| {
    thread
      .remset_drain_raw(|obj| dst.remember_from_write_barrier_buffer(obj));
  });
}

/// A `RememberedSet` adapter over the process-global remembered-set buffer.
///
/// This exists so stop-the-world GC can scan old→young edges without allocating:
/// the backing storage is a fixed-capacity global array used by the write barrier.
///
/// # Stop-the-world requirement
/// This must only be used while mutators are stopped; otherwise the write barrier could mutate the
/// global buffer concurrently.
pub(crate) struct WorldStoppedRememberedSet;

impl WorldStoppedRememberedSet {
  #[inline]
  pub(crate) fn new() -> Self {
    Self
  }
}

impl RememberedSet for WorldStoppedRememberedSet {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    let set = remembered_set();
    let len = set
      .len
      .load(Ordering::Acquire)
      .min(REMEMBERED_SET_CAPACITY);
    for i in 0..len {
      let obj = set.entries[i].load(Ordering::Acquire) as *mut u8;
      if obj.is_null() {
        continue;
      }
      if (obj as usize) % core::mem::align_of::<ObjHeader>() != 0 {
        std::process::abort();
      }
      f(obj);
    }

    // Include per-thread remembered buffers populated by registered mutators.
    registry::for_each_thread(|thread| {
      thread.remset_for_each_raw(|obj| {
        if obj.is_null() {
          return;
        }
        if (obj as usize) % core::mem::align_of::<ObjHeader>() != 0 {
          std::process::abort();
        }
        f(obj);
      });
    });
  }

  fn clear(&mut self) {
    let set = remembered_set();
    let len = set.len.swap(0, Ordering::AcqRel).min(REMEMBERED_SET_CAPACITY);
    for i in 0..len {
      let obj = set.entries[i].swap(0, Ordering::AcqRel) as *mut u8;
      if obj.is_null() {
        continue;
      }
      if (obj as usize) % core::mem::align_of::<ObjHeader>() != 0 {
        std::process::abort();
      }
      // SAFETY: Entries originate from the write barrier contract (object base pointers).
      unsafe {
        (&*(obj as *const ObjHeader)).clear_remembered_idempotent();
      }
    }

    // Clear per-thread buffers too: registered threads record remembered objects
    // in their `ThreadState` remset, not in the global buffer.
    registry::for_each_thread(|thread| {
      thread.remset_drain_raw(|obj| {
        if obj.is_null() {
          return;
        }
        if (obj as usize) % core::mem::align_of::<ObjHeader>() != 0 {
          std::process::abort();
        }
        // SAFETY: entries originate from the write barrier contract (object base pointers).
        unsafe {
          (&*(obj as *const ObjHeader)).clear_remembered_idempotent();
        }
      });
    });
  }

  fn on_promoted_object(&mut self, obj: *mut u8, has_young_refs: bool) {
    if obj.is_null() {
      return;
    }
    if (obj as usize) % core::mem::align_of::<ObjHeader>() != 0 {
      std::process::abort();
    }
    if has_young_refs {
      remset_add(obj);
    } else {
      // SAFETY: `obj` is expected to be an object base pointer.
      unsafe {
        (&*(obj as *const ObjHeader)).clear_remembered_idempotent();
      }
    }
  }
}
