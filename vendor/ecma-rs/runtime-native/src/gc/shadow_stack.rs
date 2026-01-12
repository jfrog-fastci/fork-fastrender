use core::fmt;

use smallvec::SmallVec;

use super::roots::RootSet;
use super::thread::ThreadState;

/// Per-thread shadow stack used to expose GC roots held by Rust runtime code.
///
/// Runtime-native mutator code (event loop, scheduler, string interner, etc.) is not compiled with
/// LLVM GC statepoints, so it does not have stack maps that allow the GC to find and update live
/// references held in Rust stack frames.
///
/// This shadow stack stores GC pointer *values* in a per-thread vector so the GC can enumerate and
/// update them during stop-the-world evacuation/compaction.
///
/// ## Concurrency
///
/// The owning mutator thread may push/pop roots at any time. The GC enumerates and mutates slots
/// only while the world is stopped.
pub struct ShadowStack {
  slots: std::cell::UnsafeCell<Vec<*mut u8>>,
}

impl ShadowStack {
  pub(crate) fn new(reserve_slots: usize) -> Self {
    let mut slots = Vec::new();
    slots.reserve(reserve_slots);
    Self {
      slots: std::cell::UnsafeCell::new(slots),
    }
  }

  #[inline]
  fn slots(&self) -> &Vec<*mut u8> {
    // Safety: per-thread (mutator) access, or GC access while the world is stopped.
    unsafe { &*self.slots.get() }
  }

  #[inline]
  fn slots_mut(&self) -> &mut Vec<*mut u8> {
    // Safety: per-thread (mutator) access, or GC access while the world is stopped.
    unsafe { &mut *self.slots.get() }
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.slots().len()
  }

  pub(crate) fn push(&self, ptr: *mut u8) -> usize {
    debug_assert!(
      // `gc_in_progress` is a process-global flag. Unit tests may run local `GcHeap` collections in
      // parallel without coordinating stop-the-world across the entire test runner. Guard the
      // shadow-stack invariant with the stop-the-world epoch: mutation is only forbidden while a
      // stop-the-world GC is actively running.
      !super::gc_in_progress() || crate::threading::safepoint::current_epoch() & 1 == 0,
      "cannot mutate shadow stack while stop-the-world GC is in progress"
    );
    let slots = self.slots_mut();

    if slots.len() == slots.capacity() {
      slots.reserve(1);
    }

    slots.push(ptr);
    slots.len() - 1
  }

  pub(crate) fn truncate(&self, len: usize) {
    debug_assert!(
      !super::gc_in_progress() || crate::threading::safepoint::current_epoch() & 1 == 0,
      "cannot mutate shadow stack while stop-the-world GC is in progress"
    );
    self.slots_mut().truncate(len);
  }

  pub(crate) fn get(&self, idx: usize) -> *mut u8 {
    self.slots()[idx]
  }

  pub(crate) fn set(&self, idx: usize, ptr: *mut u8) {
    debug_assert!(
      !super::gc_in_progress() || crate::threading::safepoint::current_epoch() & 1 == 0,
      "cannot mutate shadow stack while stop-the-world GC is in progress"
    );
    self.slots_mut()[idx] = ptr;
  }

  /// Return a raw pointer to the addressable slot at `idx`.
  ///
  /// # Safety notes
  /// The returned pointer is only stable as long as the current thread does not
  /// mutate the shadow stack in a way that can reallocate the underlying `Vec`
  /// (e.g. by pushing more roots). Callers should typically obtain this pointer
  /// and immediately pass it to a GC-aware API that reads from the slot after
  /// acquiring its own locks (see `roots::PersistentHandleTable::{alloc,set}_from_slot`).
  pub(crate) fn slot_ptr(&self, idx: usize) -> *mut *mut u8 {
    // Safety: per-thread access (mutator) or GC access while the world is
    // stopped. Returning a raw pointer drops the borrow immediately.
    let slots = self.slots_mut();
    (&mut slots[idx]) as *mut *mut u8
  }
}

impl fmt::Debug for ShadowStack {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ShadowStack")
      .field("len", &self.len())
      .finish()
  }
}

// Safety: `ShadowStack` is shared across threads via `ThreadState` in the global thread registry.
// The GC only touches it during stop-the-world, and only the owning mutator thread mutates it while
// running.
unsafe impl Send for ShadowStack {}
unsafe impl Sync for ShadowStack {}

impl RootSet for &ShadowStack {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    let slots = self.slots_mut();
    for slot in slots {
      f(slot as *mut *mut u8);
    }
  }
}

/// A stack-rooting scope for shadow-stack roots.
///
/// On drop, truncates the current thread's shadow stack back to the depth it had when the scope was
/// created.
pub struct RootScope<'ts> {
  ts: &'ts ThreadState,
  stack_len_at_entry: usize,
}

impl<'ts> RootScope<'ts> {
  #[inline]
  pub fn new(ts: &'ts ThreadState) -> Self {
    Self {
      ts,
      stack_len_at_entry: ts.shadow_stack().len(),
    }
  }

  #[inline]
  pub fn root<'scope>(&'scope self, ptr: *mut u8) -> RootHandle<'scope> {
    let idx = self.ts.shadow_stack().push(ptr);
    RootHandle { ts: self.ts, idx }
  }

  pub fn root_many<'scope>(&'scope self, ptrs: &[*mut u8]) -> SmallVec<[RootHandle<'scope>; 8]> {
    let mut out = SmallVec::with_capacity(ptrs.len());
    for &ptr in ptrs {
      out.push(self.root(ptr));
    }
    out
  }
}

impl Drop for RootScope<'_> {
  fn drop(&mut self) {
    self.ts.shadow_stack().truncate(self.stack_len_at_entry);
  }
}

/// A handle to a rooted GC reference.
///
/// A `RootHandle` provides indirection through the shadow stack, allowing the GC to update the
/// pointer value in-place during evacuation/compaction.
pub struct RootHandle<'scope> {
  ts: &'scope ThreadState,
  idx: usize,
}

impl<'scope> Clone for RootHandle<'scope> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<'scope> Copy for RootHandle<'scope> {}

impl RootHandle<'_> {
  #[inline]
  pub fn get(&self) -> *mut u8 {
    self.ts.shadow_stack().get(self.idx)
  }

  /// Returns a pointer to the addressable slot containing this rooted pointer.
  ///
  /// This is intended for runtime internals that need to pass an updatable slot to APIs like
  /// `PersistentHandleTable::alloc_from_slot`.
  #[inline]
  pub(crate) fn slot_ptr(&self) -> *mut *mut u8 {
    self.ts.shadow_stack().slot_ptr(self.idx)
  }

  #[inline]
  pub fn set(&self, ptr: *mut u8) {
    self.ts.shadow_stack().set(self.idx, ptr);
  }

  // Note: `slot_ptr` is intentionally `pub(crate)` (not public API). Callers must ensure the
  // underlying shadow stack vector is not reallocated while the returned pointer is in use.
}

/// Root set consisting of the shadow stacks for *all* registered threads.
///
/// This is intended to be used during stop-the-world GC: the thread registry and all shadow stacks
/// must be stable for the duration of root enumeration.
pub struct ThreadShadowStackRoots;

impl ThreadShadowStackRoots {
  pub fn new() -> Self {
    Self
  }
}

impl RootSet for ThreadShadowStackRoots {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    crate::threading::registry::for_each_thread(|ts| {
      let mut stack = ts.shadow_stack();
      stack.for_each_root_slot(f);
    });
  }
}
