use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use std::rc::Rc;
use std::sync::Arc;

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
  /// Create a `HandleId` from its raw parts.
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

  /// Alias for [`HandleId::to_u64`].
  #[inline]
  pub const fn to_raw(self) -> u64 {
    self.0
  }

  /// Alias for [`HandleId::from_u64`].
  #[inline]
  pub const fn from_raw(raw: u64) -> Self {
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

/// Internal pointer wrapper stored in the table.
///
/// On newer Rust versions, raw pointers and `NonNull<T>` are `!Send + !Sync` by default to prevent
/// accidentally building unsound safe abstractions. `HandleTable` treats pointers as opaque
/// addresses; dereferencing remains `unsafe` and the caller's responsibility.
///
/// It is therefore sound for this wrapper to be `Send + Sync` regardless of `T`.
#[derive(Copy, Clone)]
struct StoredPtr<T>(*mut T);

unsafe impl<T> Send for StoredPtr<T> {}
unsafe impl<T> Sync for StoredPtr<T> {}

impl<T> StoredPtr<T> {
  #[inline]
  fn from_nonnull(ptr: NonNull<T>) -> Self {
    Self(ptr.as_ptr())
  }

  /// # Safety
  ///
  /// The stored pointer must be non-null.
  #[inline]
  unsafe fn as_nonnull(&self) -> NonNull<T> {
    NonNull::new_unchecked(self.0)
  }
}

/// A generational, thread-safe handle table that can act as a *persistent root set*.
///
/// The table stores relocatable pointers to GC-managed objects. Host-owned queues (async tasks,
/// I/O watchers, OS event loop userdata, etc.) store [`HandleId`] values instead of direct pointers,
/// allowing the GC to move objects and update table entries during compaction.
///
/// # Concurrency
///
/// - [`HandleTable::get`] takes a shared lock (`RwLock` read).
/// - Allocation/freeing takes an exclusive lock (`RwLock` write).
///
/// # Stop-the-world relocation updates
///
/// Moving collectors must update handle table pointers only when all mutator threads are parked.
/// Use [`HandleTable::with_stw_update`]; this API does **not** stop threads itself.
pub struct HandleTable<T> {
  inner: RwLock<HandleTableInner<T>>,
}

impl<T> Default for HandleTable<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> fmt::Debug for HandleTable<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let inner = self.inner.read();
    f.debug_struct("HandleTable")
      .field("slots", &inner.slots.len())
      .field("free_head", &inner.free_head)
      .finish()
  }
}

impl<T> HandleTable<T> {
  /// Creates an empty handle table.
  #[inline]
  pub fn new() -> Self {
    Self {
      inner: RwLock::new(HandleTableInner {
        slots: Vec::new(),
        free_head: None,
      }),
    }
  }

  /// Allocates a new handle for `ptr` and returns its stable [`HandleId`].
  ///
  /// The allocated entry is considered a **GC root** until it is freed via [`HandleTable::free`].
  pub fn alloc(&self, ptr: NonNull<T>) -> HandleId {
    let mut inner = self.inner.write();
    let HandleTableInner { slots, free_head } = &mut *inner;

    if let Some(index) = *free_head {
      let slot = &mut slots[index as usize];
      let generation = match slot {
        Slot::Free {
          next_free,
          generation,
        } => {
          *free_head = *next_free;
          *generation
        }
        Slot::Live { .. } => unreachable!("free list points at a live slot"),
      };

      *slot = Slot::Live {
        ptr: StoredPtr::from_nonnull(ptr),
        generation,
      };

      return HandleId::from_parts(index, generation);
    }

    let index: u32 = slots
      .len()
      .try_into()
      .expect("HandleTable index overflow (more than u32::MAX slots)");

    // Start generations at 1 so HandleId(0) can be used as a sentinel (e.g. empty userdata).
    let generation = 1;

    slots.push(Slot::Live {
      ptr: StoredPtr::from_nonnull(ptr),
      generation,
    });

    HandleId::from_parts(index, generation)
  }

  /// Returns the current pointer for `id` if it is still live.
  #[inline]
  pub fn get(&self, id: HandleId) -> Option<NonNull<T>> {
    let inner = self.inner.read();
    let slot = inner.slots.get(id.index() as usize)?;
    match slot {
      Slot::Live { ptr, generation } if *generation == id.generation() => {
        // Safety: all live slots are stored as non-null pointers.
        Some(unsafe { ptr.as_nonnull() })
      }
      _ => None,
    }
  }

  /// Update the pointer stored in `id`'s slot.
  ///
  /// Returns `true` if `id` was live and successfully updated.
  pub fn set(&self, id: HandleId, ptr: NonNull<T>) -> bool {
    let mut inner = self.inner.write();
    let slot = match inner.slots.get_mut(id.index() as usize) {
      Some(slot) => slot,
      None => return false,
    };

    match slot {
      Slot::Live {
        ptr: stored_ptr,
        generation,
      } if *generation == id.generation() => {
        *stored_ptr = StoredPtr::from_nonnull(ptr);
        true
      }
      _ => false,
    }
  }

  /// Frees `id`, removing it from the persistent root set and making its slot reusable.
  ///
  /// Returns the stored pointer if the handle was live.
  pub fn free(&self, id: HandleId) -> Option<NonNull<T>> {
    let mut inner = self.inner.write();
    let HandleTableInner { slots, free_head } = &mut *inner;

    let slot = slots.get_mut(id.index() as usize)?;

    match slot {
      Slot::Live { ptr, generation } if *generation == id.generation() => {
        // Safety: all live slots are stored as non-null pointers.
        let old_ptr = unsafe { ptr.as_nonnull() };

        // Bump generation to invalidate stale IDs.
        let mut new_generation = generation.wrapping_add(1);
        if new_generation == 0 {
          // Keep 0 reserved for sentinel use.
          new_generation = 1;
        }

        *slot = Slot::Free {
          next_free: *free_head,
          generation: new_generation,
        };
        *free_head = Some(id.index());

        Some(old_ptr)
      }
      _ => None,
    }
  }

  /// Stop-the-world (STW) update hook for moving/compacting GC relocation.
  ///
  /// This method takes an exclusive lock and exposes mutable access to all live slot pointers
  /// through the provided guard.
  ///
  /// # Important: caller must already be in STW
  ///
  /// This API **does not** itself park/stop other mutator threads. The caller must ensure that:
  /// - no other thread will call [`HandleTable::get`] / [`HandleTable::alloc`] / [`HandleTable::free`]
  ///   while the closure runs, and
  /// - no thread is currently blocked holding a read lock when entering STW (otherwise this can
  ///   deadlock waiting for the write lock).
  pub fn with_stw_update<R>(&self, f: impl FnOnce(&mut HandleTableStwGuard<'_, T>) -> R) -> R {
    let guard = self.inner.write();
    let mut guard = HandleTableStwGuard { guard };
    f(&mut guard)
  }
}

struct HandleTableInner<T> {
  slots: Vec<Slot<T>>,
  free_head: Option<u32>,
}

enum Slot<T> {
  Free {
    next_free: Option<u32>,
    generation: u32,
  },
  Live {
    ptr: StoredPtr<T>,
    generation: u32,
  },
}

/// Guard object passed to [`HandleTable::with_stw_update`].
pub struct HandleTableStwGuard<'a, T> {
  guard: RwLockWriteGuard<'a, HandleTableInner<T>>,
}

impl<'a, T> HandleTableStwGuard<'a, T> {
  /// Iterates over the raw pointers stored in all currently-live slots.
  ///
  /// Each returned `&mut *mut T` may be rewritten by the caller to point at the object's new
  /// location after relocation.
  ///
  /// The pointer must remain non-null.
  pub fn iter_live_slots_mut(&mut self) -> impl Iterator<Item = &mut *mut T> + '_ {
    self.guard.slots.iter_mut().filter_map(|slot| match slot {
      Slot::Live { ptr, .. } => Some(&mut ptr.0),
      Slot::Free { .. } => None,
    })
  }

  /// Like [`HandleTableStwGuard::iter_live_slots_mut`], but also yields the corresponding live
  /// [`HandleId`].
  pub fn iter_live_mut(&mut self) -> impl Iterator<Item = (HandleId, &mut *mut T)> + '_ {
    self.guard
      .slots
      .iter_mut()
      .enumerate()
      .filter_map(|(index, slot)| match slot {
        Slot::Live { ptr, generation } => {
          let index: u32 = index
            .try_into()
            .expect("HandleTable index overflow (more than u32::MAX slots)");
          Some((HandleId::from_parts(index, *generation), &mut ptr.0))
        }
        Slot::Free { .. } => None,
      })
  }
}

/// RAII wrapper for a persistent handle created by [`HandleTable::alloc`].
///
/// This is intended for host code that wants to avoid leaking handles on early returns.
///
/// For long-lived handles stored in host state (queued async work, OS event loop userdata, etc.),
/// prefer storing the returned [`HandleId`] from [`HandleTable::alloc`] directly and calling
/// [`HandleTable::free`] explicitly when done.
#[must_use]
pub struct PersistentHandle<'a, T> {
  table: &'a HandleTable<T>,
  id: HandleId,

  // `PersistentHandle` is intentionally `!Send`/`!Sync` by default.
  _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'a, T> PersistentHandle<'a, T> {
  /// Allocates a new persistent handle and returns an RAII guard that frees it on drop.
  pub fn new(table: &'a HandleTable<T>, ptr: NonNull<T>) -> Self {
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

  /// Borrows the underlying table.
  #[inline]
  pub fn table(&self) -> &'a HandleTable<T> {
    self.table
  }
}

impl<T> Drop for PersistentHandle<'_, T> {
  fn drop(&mut self) {
    let _ = self.table.free(self.id);
  }
}

/// An owned persistent handle that can be stored in host queues.
///
/// Unlike [`PersistentHandle`], this type does not borrow the handle table; instead it keeps a
/// shared reference to a mutex-protected table so it can be moved into long-lived host state (async
/// tasks, I/O watchers, timers).
#[must_use]
pub struct OwnedGcHandle<T> {
  table: Arc<Mutex<HandleTable<T>>>,
  id: Option<HandleId>,
}

impl<T> OwnedGcHandle<T> {
  /// Allocates a new persistent handle in `table`.
  pub fn new(table: Arc<Mutex<HandleTable<T>>>, ptr: NonNull<T>) -> Self {
    let id = table.lock().alloc(ptr);
    Self {
      table,
      id: Some(id),
    }
  }

  /// The underlying stable [`HandleId`].
  #[inline]
  pub fn id(&self) -> HandleId {
    self.id.expect("OwnedGcHandle already released")
  }

  /// Releases this handle table entry, removing it from the persistent root set.
  #[inline]
  pub fn release(mut self) {
    if let Some(id) = self.id.take() {
      self.table.lock().free(id);
    }
  }
}

impl<T> Drop for OwnedGcHandle<T> {
  fn drop(&mut self) {
    let Some(id) = self.id.take() else {
      return;
    };
    self.table.lock().free(id);
  }
}
