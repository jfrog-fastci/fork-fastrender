use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::rc::Rc;

/// A stable identifier for an entry in a [`HandleTable`].
///
/// This is a packed `{ index: u32, generation: u32 }`.
/// - `index` selects a slot in the table's slot vector.
/// - `generation` is incremented each time that slot is freed.
///
/// A `HandleId` is **only valid** if:
/// - `index` is in-bounds for the current table,
/// - the slot at `index` is occupied, and
/// - the slot's generation matches this handle's generation.
///
/// The compact `u64` representation is intended to be stored in OS event-loop userdata (epoll,
/// kqueue, IOCP, ...).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct HandleId(u64);

impl HandleId {
  #[inline]
  pub const fn from_parts(index: u32, generation: u32) -> Self {
    Self((index as u64) | ((generation as u64) << 32))
  }

  /// The slot index within the handle table.
  #[inline]
  pub const fn index(self) -> u32 {
    self.0 as u32
  }

  /// The generation of the slot when this handle was created.
  #[inline]
  pub const fn generation(self) -> u32 {
    (self.0 >> 32) as u32
  }

  /// Converts this ID to its compact `u64` representation.
  #[inline]
  pub const fn to_u64(self) -> u64 {
    self.0
  }

  /// Recreates a `HandleId` from its compact `u64` representation.
  #[inline]
  pub const fn from_u64(raw: u64) -> Self {
    Self(raw)
  }
}

impl From<u64> for HandleId {
  #[inline]
  fn from(value: u64) -> Self {
    Self::from_u64(value)
  }
}

impl From<HandleId> for u64 {
  #[inline]
  fn from(value: HandleId) -> Self {
    value.to_u64()
  }
}

impl fmt::Debug for HandleId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("HandleId")
      .field("index", &self.index())
      .field("generation", &self.generation())
      .finish()
  }
}

#[derive(Clone, Copy)]
struct Slot<T> {
  generation: u32,
  // Null pointer indicates a free slot.
  ptr: *mut T,
}

/// A generational handle table that can act as a *persistent root set*.
///
/// The table stores relocatable pointers to GC-managed objects. Host-owned work queues store
/// [`HandleId`] values instead of direct pointers, allowing the GC to move objects and update the
/// table entries during compaction.
///
/// ## Stop-the-world relocation updates
///
/// [`HandleTable::update`] and [`HandleTable::iter_live_mut`] are intended to be used by a moving
/// collector to update pointers after relocating objects. These operations assume a stop-the-world
/// (STW) pause: all mutator threads must be parked while relocation updates run.
pub struct HandleTable<T> {
  slots: Vec<Slot<T>>,
  free_list: Vec<u32>,
}

// SAFETY: `HandleTable` stores raw pointers as opaque values. It has no interior mutability of its
// own; concurrent access must be synchronized externally (e.g. by the GC's stop-the-world pause or
// a mutex). This mirrors the safety rationale used by other pointer tables in this crate (e.g.
// `WeakHandles`).
unsafe impl<T> Send for HandleTable<T> {}

impl<T> Default for HandleTable<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> fmt::Debug for HandleTable<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("HandleTable")
      .field("slots", &self.slots.len())
      .field("free_list", &self.free_list.len())
      .finish()
  }
}

impl<T> HandleTable<T> {
  /// Creates an empty handle table.
  #[inline]
  pub fn new() -> Self {
    Self {
      slots: Vec::new(),
      free_list: Vec::new(),
    }
  }

  /// Allocates a new handle for `ptr` and returns its stable [`HandleId`].
  ///
  /// The allocated entry is considered a **GC root** until it is freed via [`HandleTable::free`].
  pub fn alloc(&mut self, ptr: NonNull<T>) -> HandleId {
    if let Some(index) = self.free_list.pop() {
      let slot = &mut self.slots[index as usize];
      debug_assert!(slot.ptr.is_null(), "free_list points at a live slot");
      slot.ptr = ptr.as_ptr();
      HandleId::from_parts(index, slot.generation)
    } else {
      let index: u32 = self
        .slots
        .len()
        .try_into()
        .expect("HandleTable index overflow (more than u32::MAX slots)");
      self.slots.push(Slot {
        generation: 0,
        ptr: ptr.as_ptr(),
      });
      HandleId::from_parts(index, 0)
    }
  }

  /// Returns the current pointer for `id` if it is still live.
  #[inline]
  pub fn get(&self, id: HandleId) -> Option<NonNull<T>> {
    let slot = self.slots.get(id.index() as usize)?;
    (slot.generation == id.generation()).then_some(())?;
    NonNull::new(slot.ptr)
  }

  /// Updates the pointer stored for `id`.
  ///
  /// This is intended for use by a moving GC when relocating objects during an STW pause.
  ///
  /// Returns `true` if the handle was live and successfully updated.
  pub fn update(&mut self, id: HandleId, new_ptr: NonNull<T>) -> bool {
    let Some(slot) = self.slots.get_mut(id.index() as usize) else {
      return false;
    };
    if slot.generation != id.generation() {
      return false;
    }
    if slot.ptr.is_null() {
      return false;
    };
    slot.ptr = new_ptr.as_ptr();
    true
  }

  /// Frees `id`, removing it from the persistent root set and making its slot reusable.
  ///
  /// Returns `true` if the handle was live and successfully freed.
  pub fn free(&mut self, id: HandleId) -> bool {
    let Some(slot) = self.slots.get_mut(id.index() as usize) else {
      return false;
    };
    if slot.generation != id.generation() {
      return false;
    }
    if slot.ptr.is_null() {
      return false;
    }
    slot.ptr = core::ptr::null_mut();

    slot.generation = slot.generation.wrapping_add(1);
    self.free_list.push(id.index());
    true
  }

  /// Iterates over all live entries, yielding a mutable reference to each pointer slot.
  ///
  /// This is intended for bulk relocation updates by a moving GC during an STW pause.
  pub fn iter_live_mut(&mut self) -> impl Iterator<Item = (HandleId, &mut *mut T)> {
    self.slots.iter_mut().enumerate().filter_map(|(index, slot)| {
      let generation = slot.generation;
      if slot.ptr.is_null() {
        return None;
      }
      let ptr = &mut slot.ptr;
      let index: u32 = index
        .try_into()
        .expect("HandleTable index overflow (more than u32::MAX slots)");
      Some((HandleId::from_parts(index, generation), ptr))
    })
  }
}

/// RAII wrapper for a persistent handle created by [`HandleTable::alloc`].
///
/// This is intended for host code that wants to avoid leaking handles on early returns.
///
/// While this guard is alive it holds a mutable borrow of the [`HandleTable`]. For long-lived
/// handles stored in host state (queued async work, OS event loop userdata, etc.), prefer storing
/// the returned [`HandleId`] from [`HandleTable::alloc`] directly and calling
/// [`HandleTable::free`] explicitly when done.
#[must_use]
pub struct PersistentHandle<'a, T> {
  table: &'a mut HandleTable<T>,
  id: HandleId,

  // `PersistentHandle` is intentionally `!Send`/`!Sync` by default.
  _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'a, T> PersistentHandle<'a, T> {
  /// Allocates a new persistent handle and returns an RAII guard that frees it on drop.
  pub fn new(table: &'a mut HandleTable<T>, ptr: NonNull<T>) -> Self {
    let id = table.alloc(ptr);
    Self {
      table,
      id,
      _not_send_or_sync: PhantomData,
    }
  }

  /// The underlying [`HandleId`].
  #[inline]
  pub fn id(&self) -> HandleId {
    self.id
  }

  /// Returns the current pointer for this handle, if still live.
  #[inline]
  pub fn get(&self) -> Option<NonNull<T>> {
    self.table.get(self.id)
  }

  /// Updates the pointer stored for this handle.
  ///
  /// Returns `true` if the handle was live and successfully updated.
  #[inline]
  pub fn update(&mut self, new_ptr: NonNull<T>) -> bool {
    self.table.update(self.id, new_ptr)
  }

  /// Borrows the underlying table immutably.
  #[inline]
  pub fn table(&self) -> &HandleTable<T> {
    &*self.table
  }

  /// Borrows the underlying table mutably.
  #[inline]
  pub fn table_mut(&mut self) -> &mut HandleTable<T> {
    &mut *self.table
  }
}

impl<T> Drop for PersistentHandle<'_, T> {
  fn drop(&mut self) {
    self.table.free(self.id);
  }
}
