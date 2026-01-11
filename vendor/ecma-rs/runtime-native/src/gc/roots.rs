use core::fmt;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering;

/// A precise root set enumerator.
///
/// The slots passed to the callback are mutable pointers to GC references
/// (`*mut u8`). A GC implementation may update the slot in-place (e.g. when
/// evacuating a young object).
pub trait RootSet {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8));
}

/// Simple root set implementation used by unit tests.
#[derive(Default)]
pub struct RootStack {
  slots: Vec<*mut *mut u8>,
}

impl RootStack {
  pub fn new() -> Self {
    Self { slots: Vec::new() }
  }

  pub fn push(&mut self, slot: *mut *mut u8) {
    self.slots.push(slot);
  }

  pub fn pop(&mut self) -> *mut *mut u8 {
    self.slots.pop().expect("RootStack::pop: empty root stack")
  }
}

impl RootSet for RootStack {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for &slot in &self.slots {
      f(slot);
    }
  }
}

/// Old-to-young remembered set, parameterized by write-barrier policy.
pub trait RememberedSet {
  /// Enumerate old-generation objects that may contain pointers into the nursery.
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8));

  /// Clear all remembered entries.
  fn clear(&mut self);

  fn on_promoted_object(&mut self, obj: *mut u8, has_young_refs: bool);
}

#[derive(Default)]
pub struct SimpleRememberedSet {
  objs: Vec<*mut u8>,
}

// SAFETY: `SimpleRememberedSet` stores raw pointers which are treated as opaque
// addresses. The set is intended to be used under stop-the-world GC or external
// synchronization (e.g. a mutex in the exported write barrier). It does not
// dereference the pointers except while the world is stopped.
unsafe impl Send for SimpleRememberedSet {}

impl SimpleRememberedSet {
  pub fn new() -> Self {
    Self::default()
  }

  /// Test-only helper: drop all remembered-set entries without touching the objects.
  ///
  /// Integration tests often allocate ad-hoc "fake" objects (plain `Box<T>` or raw `alloc_zeroed`)
  /// and then call exported barriers on them. Those objects are freed by the test harness, so the
  /// runtime must not dereference remembered-set pointers during global test cleanup.
  ///
  /// This method is intentionally *weaker* than [`RememberedSet::clear`]: it only forgets the
  /// addresses and does **not** clear the per-object `REMEMBERED` header bit.
  #[doc(hidden)]
  pub fn clear_for_tests(&mut self) {
    self.objs.clear();
  }

  /// Record an old-generation object as potentially containing young-generation pointers.
  ///
  /// This is intended for use by write barriers. The object is added at most once; the per-object
  /// `REMEMBERED` header bit is used to deduplicate.
  pub fn remember(&mut self, obj: *mut u8) {
    self.add(obj);
  }

  pub fn contains(&self, obj: *mut u8) -> bool {
    self.objs.contains(&obj)
  }

  fn add(&mut self, obj: *mut u8) {
    debug_assert!(!obj.is_null());
    // Set the per-object REMEMBERED bit idempotently and only enqueue into the
    // remembered set the first time it transitions from 0 -> 1.
    //
    // SAFETY: `obj` must point to the start of a valid GC-managed object.
    let header = unsafe { &*(obj as *const super::ObjHeader) };
    debug_assert!(
      !header.is_forwarded(),
      "attempted to remember a forwarded (nursery) object"
    );
    if !header.set_remembered_idempotent() {
      return;
    }
    self.objs.push(obj);
  }

  fn remove(&mut self, obj: *mut u8) {
    if obj.is_null() {
      return;
    }
    // SAFETY: `obj` must point to the start of a valid GC-managed object.
    let header = unsafe { &*(obj as *const super::ObjHeader) };
    header.clear_remembered_idempotent();

    if let Some(idx) = self.objs.iter().position(|&x| x == obj) {
      self.objs.swap_remove(idx);
    }
  }

  pub fn scan_and_rebuild(&mut self, mut object_has_young_refs: impl FnMut(*mut u8) -> bool) {
    let mut new = Vec::with_capacity(self.objs.len());
    for &obj in &self.objs {
      if object_has_young_refs(obj) {
        new.push(obj);
      } else {
        // SAFETY: `obj` must point to the start of a valid GC-managed object.
        unsafe { (&*(obj as *const super::ObjHeader)).clear_remembered_idempotent() };
      }
    }
    self.objs = new;
  }
}

impl RememberedSet for SimpleRememberedSet {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    for &obj in &self.objs {
      // SAFETY: `obj` must point to the start of a valid GC-managed object.
      unsafe { (&*(obj as *const super::ObjHeader)).clear_remembered_idempotent() };
    }
    self.objs.clear();
  }

  fn on_promoted_object(&mut self, obj: *mut u8, has_young_refs: bool) {
    if has_young_refs {
      self.add(obj);
    } else {
      self.remove(obj);
    }
  }
}

/// A stable identifier for a persistent GC root stored in a [`RootHandles`] table.
///
/// This is a packed `{ index: u32, generation: u32 }`.
/// - `index` selects a slot in the handle table.
/// - `generation` is incremented each time that slot is removed and reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct RootHandle(u64);

impl RootHandle {
  #[inline]
  pub fn from_parts(index: u32, generation: u32) -> Self {
    Self((index as u64) | ((generation as u64) << 32))
  }

  #[inline]
  pub fn from_u64(raw: u64) -> Self {
    Self(raw)
  }

  #[inline]
  pub fn as_u64(self) -> u64 {
    self.0
  }

  #[inline]
  pub fn index(self) -> u32 {
    self.0 as u32
  }

  #[inline]
  pub fn generation(self) -> u32 {
    (self.0 >> 32) as u32
  }
}

impl fmt::Debug for RootHandle {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("RootHandle")
      .field("index", &self.index())
      .field("generation", &self.generation())
      .finish()
  }
}

struct RootSlot {
  value: AtomicPtr<u8>,
  generation: u32,
}

impl RootSlot {
  #[inline]
  fn is_free(&self) -> bool {
    self.value.load(Ordering::Relaxed).is_null()
  }
}

/// Handle table for persistent roots.
///
/// This is intended for runtime subsystems and host/FFI code that need to keep
/// GC objects alive across safepoints and moving collections.
pub struct RootHandles {
  slots: Vec<RootSlot>,
  free_list: Vec<u32>,
}

impl Default for RootHandles {
  fn default() -> Self {
    Self::new()
  }
}

impl RootHandles {
  pub fn new() -> Self {
    Self {
      slots: Vec::new(),
      free_list: Vec::new(),
    }
  }

  pub fn root_add(&mut self, value: *mut u8) -> RootHandle {
    let index = if let Some(index) = self.free_list.pop() {
      index as usize
    } else {
      let index = self.slots.len();
      self.slots.push(RootSlot {
        value: AtomicPtr::new(core::ptr::null_mut()),
        generation: 0,
      });
      index
    };

    let slot = &mut self.slots[index];
    slot.value.store(value, Ordering::Relaxed);
    RootHandle::from_parts(index as u32, slot.generation)
  }

  pub fn root_get(&self, h: RootHandle) -> Option<*mut u8> {
    let slot = self.slots.get(h.index() as usize)?;
    if slot.generation != h.generation() || slot.is_free() {
      return None;
    };
    Some(slot.value.load(Ordering::Relaxed))
  }

  pub fn root_set(&mut self, h: RootHandle, value: *mut u8) {
    let Some(slot) = self.slots.get_mut(h.index() as usize) else {
      return;
    };
    if slot.generation != h.generation() || slot.is_free() {
      return;
    }
    if value.is_null() {
      slot.value.store(core::ptr::null_mut(), Ordering::Relaxed);
      slot.generation = slot.generation.wrapping_add(1);
      self.free_list.push(h.index());
      return;
    }
    slot.value.store(value, Ordering::Relaxed);
  }

  pub fn root_remove(&mut self, h: RootHandle) {
    let Some(slot) = self.slots.get_mut(h.index() as usize) else {
      return;
    };
    if slot.generation != h.generation() || slot.is_free() {
      return;
    }
    slot.value.store(core::ptr::null_mut(), Ordering::Relaxed);
    slot.generation = slot.generation.wrapping_add(1);
    self.free_list.push(h.index());
  }
}

impl RootSet for RootHandles {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for slot in &self.slots {
      if slot.is_free() {
        continue;
      }
      // Expose a raw pointer to the stored GC reference so tracing/evacuation
      // can update it in place.
      f(slot.value.as_ptr());
    }
  }
}
