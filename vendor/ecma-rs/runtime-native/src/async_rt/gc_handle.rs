use core::ptr::NonNull;
use std::fmt;
use std::marker::PhantomData;

use crate::gc::{HandleId, HandleTable};

/// A copyable, typed handle to an entry in a [`HandleTable`].
///
/// This is the type intended to be stored in:
/// - lock-free ready queues,
/// - OS userdata (epoll/kqueue/io_uring) as a `u64`,
/// - any host-owned state that must not hold raw GC pointers.
///
/// The handle is **non-owning**: it does not free the table slot on drop.
#[repr(transparent)]
#[derive(PartialEq, Eq, Hash)]
pub struct AsyncHandle<T> {
  id: HandleId,
  _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AsyncHandle<T> {}

impl<T> Clone for AsyncHandle<T> {
  #[inline]
  fn clone(&self) -> Self {
    *self
  }
}

impl<T> AsyncHandle<T> {
  #[inline]
  pub const fn from_handle_id(id: HandleId) -> Self {
    Self {
      id,
      _marker: PhantomData,
    }
  }

  #[inline]
  pub const fn handle_id(self) -> HandleId {
    self.id
  }

  #[inline]
  pub const fn from_raw(raw: u64) -> Self {
    Self::from_handle_id(HandleId::from_u64(raw))
  }

  #[inline]
  pub const fn into_raw(self) -> u64 {
    self.id.to_u64()
  }
}

impl<T> fmt::Debug for AsyncHandle<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("AsyncHandle")
      .field("raw", &self.into_raw())
      .field("id", &self.id)
      .finish()
  }
}

impl<T> From<HandleId> for AsyncHandle<T> {
  #[inline]
  fn from(value: HandleId) -> Self {
    Self::from_handle_id(value)
  }
}

impl<T> From<AsyncHandle<T>> for HandleId {
  #[inline]
  fn from(value: AsyncHandle<T>) -> Self {
    value.handle_id()
  }
}

impl<T> From<u64> for AsyncHandle<T> {
  #[inline]
  fn from(value: u64) -> Self {
    Self::from_raw(value)
  }
}

impl<T> From<AsyncHandle<T>> for u64 {
  #[inline]
  fn from(value: AsyncHandle<T>) -> Self {
    value.into_raw()
  }
}

/// An owning handle table entry.
///
/// On creation, this allocates a slot in the [`HandleTable`]. The slot is freed
/// on [`OwnedAsyncHandle::discard`] or on `Drop`.
///
/// ## Intended async-runtime pattern
///
/// - When a coroutine/task parks, create an `OwnedAsyncHandle<CoroutineFrame>` and
///   store it in the runtime's owned state for the duration of the suspension.
/// - Store a copyable [`AsyncHandle`] (or its `u64` form) in ready queues / OS
///   userdata.
/// - On wake/run/cancel/shutdown, call [`OwnedAsyncHandle::discard`] exactly once
///   to free the persistent root.
pub struct OwnedAsyncHandle<'table, T> {
  table: &'table HandleTable<T>,
  id: HandleId,
  discarded: bool,
}

impl<'table, T> OwnedAsyncHandle<'table, T> {
  /// Allocate a new handle table entry for `ptr`.
  ///
  /// The handle table stores a stable, relocatable pointer to a GC-managed
  /// object. The pointee is **not** owned by the handle table.
  pub fn new(table: &'table HandleTable<T>, ptr: NonNull<T>) -> Self {
    let id = table.alloc_movable(ptr);
    Self {
      table,
      id,
      discarded: false,
    }
  }

  /// Allocate a new handle table entry for `ptr`.
  ///
  /// # Safety
  /// `ptr` must be non-null and point to a valid GC-managed object.
  pub unsafe fn new_unchecked(table: &'table HandleTable<T>, ptr: *mut T) -> Self {
    debug_assert!(!ptr.is_null(), "OwnedAsyncHandle::new_unchecked: ptr must not be null");
    Self::new(table, NonNull::new_unchecked(ptr))
  }

  /// Returns the non-owning handle for this entry.
  #[inline]
  pub fn raw(&self) -> AsyncHandle<T> {
    AsyncHandle::from_handle_id(self.id)
  }

  /// Convenience wrapper around [`OwnedAsyncHandle::raw`].
  #[inline]
  pub fn with_raw<R>(&self, f: impl FnOnce(AsyncHandle<T>) -> R) -> R {
    f(self.raw())
  }

  /// Free the underlying handle table entry.
  pub fn discard(mut self) {
    self.discard_in_place();
  }

  fn discard_in_place(&mut self) {
    if self.discarded {
      return;
    }

    // Ignore failure: a generational handle might have been invalidated already
    // by the runtime (e.g. if ownership was transferred), and discard should be
    // idempotent.
    let _ = self.table.free(self.id);
    self.discarded = true;
  }
}

impl<T> fmt::Debug for OwnedAsyncHandle<'_, T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("OwnedAsyncHandle")
      .field("id", &self.id)
      .field("discarded", &self.discarded)
      .finish()
  }
}

impl<T> Drop for OwnedAsyncHandle<'_, T> {
  fn drop(&mut self) {
    self.discard_in_place();
  }
}

/// A debug-only "must discard" wrapper.
///
/// In debug builds this asserts if dropped without an explicit call to
/// [`MustDiscardOwnedAsyncHandle::discard`]. This mimics `vm-js::Job`'s discipline
/// when you want to ensure control flow never silently drops queued work.
pub struct MustDiscardOwnedAsyncHandle<'table, T> {
  inner: OwnedAsyncHandle<'table, T>,
  explicitly_discarded: bool,
}

impl<'table, T> MustDiscardOwnedAsyncHandle<'table, T> {
  pub fn new(table: &'table HandleTable<T>, ptr: NonNull<T>) -> Self {
    Self {
      inner: OwnedAsyncHandle::new(table, ptr),
      explicitly_discarded: false,
    }
  }

  #[inline]
  pub fn raw(&self) -> AsyncHandle<T> {
    self.inner.raw()
  }

  #[inline]
  pub fn with_raw<R>(&self, f: impl FnOnce(AsyncHandle<T>) -> R) -> R {
    self.inner.with_raw(f)
  }

  pub fn discard(mut self) {
    self.explicitly_discarded = true;
    self.inner.discard_in_place();
  }
}

impl<T> Drop for MustDiscardOwnedAsyncHandle<'_, T> {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      self.inner.discard_in_place();
      return;
    }

    if !self.explicitly_discarded {
      // Free first so even a debug assertion doesn't leak table slots.
      self.inner.discard_in_place();
      debug_assert!(
        false,
        "MustDiscardOwnedAsyncHandle dropped without discard(); call discard() explicitly"
      );
    }
  }
}
