use core::alloc::Layout;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::alloc::{alloc, alloc_zeroed, dealloc};

/// Minimum alignment guaranteed for all `ArrayBuffer` backing store allocations.
///
/// Rationale:
/// - Ensures stable, reasonably aligned pointers for kernel I/O.
/// - Leaves room for future SIMD/vectorized operations on typed array data.
pub const BACKING_STORE_MIN_ALIGN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingStoreAllocError {
  /// The requested size/align was not representable as a `Layout` (overflow or invalid).
  InvalidLayout,
  /// The allocator returned a null pointer.
  OutOfMemory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingStorePinError {
  /// Backing store has been detached/finalized (or is in the middle of detaching/freeing).
  NotAlive,
  /// Requested pin range exceeded the visible `byte_len`.
  OutOfBounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingStoreDetachError {
  /// Backing store is currently pinned (in-flight).
  Pinned,
  /// Backing store has already been detached/finalized (or is in the middle of detaching/freeing).
  NotAlive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BackingStoreState {
  Alive = 0,
  Detaching = 1,
  Freeing = 2,
  /// Backing store was requested to be freed, but was pinned at the time.
  ///
  /// The final deallocation happens when the last [`PinnedBackingStore`] guard is dropped.
  FreePending = 3,
}

impl BackingStoreState {
  #[inline]
  fn from_u8(value: u8) -> Self {
    match value {
      0 => Self::Alive,
      1 => Self::Detaching,
      2 => Self::Freeing,
      3 => Self::FreePending,
      _ => Self::FreePending,
    }
  }
}

#[derive(Debug)]
struct BackingStoreCtl {
  state: AtomicU8,
  pin_count: AtomicUsize,
  alloc_ptr: *mut u8,
  alloc_len: usize,
  alloc_align: usize,
  // Points at the owning allocator's external-bytes counter so we can keep accounting correct even
  // when the actual free happens after `BackingStoreAllocator::free` returns (due to pinning).
  external_bytes: *const AtomicUsize,
}

impl BackingStoreCtl {
  #[inline]
  fn state(&self) -> BackingStoreState {
    BackingStoreState::from_u8(self.state.load(Ordering::Acquire))
  }
}

unsafe fn finalize_ctl(ctl: NonNull<BackingStoreCtl>) {
  let ctl_ref = ctl.as_ref();

  if ctl_ref.alloc_len != 0 {
    // We only ever allocate with (1) alignment 1 via Vec/Box or (2) alignment BACKING_STORE_MIN_ALIGN.
    debug_assert!(ctl_ref.alloc_align == 1 || ctl_ref.alloc_align == BACKING_STORE_MIN_ALIGN);

    let layout = Layout::from_size_align(ctl_ref.alloc_len, ctl_ref.alloc_align)
      .expect("valid layout for backing store dealloc");
    dealloc(ctl_ref.alloc_ptr, layout);
  }

  if !ctl_ref.external_bytes.is_null() && ctl_ref.alloc_len != 0 {
    let prev = (*ctl_ref.external_bytes).fetch_sub(ctl_ref.alloc_len, Ordering::Relaxed);
    debug_assert!(
      prev >= ctl_ref.alloc_len,
      "external_bytes underflow (prev={prev}, sub={})",
      ctl_ref.alloc_len
    );
  }

  drop(Box::from_raw(ctl.as_ptr()));
}

/// RAII guard representing an in-flight use of a backing store.
///
/// While this guard is alive, detaching/freeing the backing store is rejected and the final
/// deallocation (if requested while pinned) is delayed until the guard drops.
#[derive(Debug)]
pub struct PinnedBackingStore {
  ctl: NonNull<BackingStoreCtl>,
}

impl Drop for PinnedBackingStore {
  fn drop(&mut self) {
    // SAFETY: `ctl` remains valid until either:
    // - this drop observes it is the last pin and frees it, or
    // - a successful detach/free frees it (which can only happen when pin_count == 0).
    let ctl_ref = unsafe { self.ctl.as_ref() };
    let prev = ctl_ref.pin_count.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(prev > 0, "pin_count underflow");

    if prev == 1 {
      // Last pin dropped.
      if ctl_ref.state() == BackingStoreState::FreePending {
        // Claim the finalization so only one thread frees.
        if ctl_ref
          .state
          .compare_exchange(
            BackingStoreState::FreePending as u8,
            BackingStoreState::Freeing as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
          )
          .is_ok()
        {
          unsafe { finalize_ctl(self.ctl) };
        }
      }
    }
  }
}

/// A stable, non-moving byte buffer used as the backing store for `ArrayBuffer`/`TypedArray`.
///
/// This object is designed to be stored inside *movable* GC-managed headers:
/// it does not run destructors and can be relocated via `memcpy` without invalidating the
/// underlying byte pointer.
///
/// # Safety / invariants
/// - When `alloc_len == 0`, `ptr` is a dangling non-null pointer and must not be dereferenced.
/// - When `alloc_len > 0`, `ptr` points to `alloc_len` bytes allocated with `alloc_align`.
/// - `ptr` is always aligned to at least [`BACKING_STORE_MIN_ALIGN`].
#[derive(Debug)]
#[repr(C)]
pub struct BackingStore {
  ptr: NonNull<u8>,
  /// Visible length in bytes (ArrayBuffer `byteLength`).
  byte_len: usize,
  /// Actual allocation size in bytes. For adopted `Vec<u8>` this may be `capacity`.
  alloc_len: usize,
  /// Allocation alignment used for deallocation. For adopted `Vec<u8>` / `Box<[u8]>` this is 1.
  alloc_align: usize,
  ctl: *mut BackingStoreCtl,
}

impl BackingStore {
  #[inline]
  pub fn empty() -> Self {
    Self {
      ptr: NonNull::dangling(),
      byte_len: 0,
      alloc_len: 0,
      alloc_align: BACKING_STORE_MIN_ALIGN,
      ctl: core::ptr::null_mut(),
    }
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.alloc_len == 0
  }

  #[inline]
  pub fn byte_len(&self) -> usize {
    self.byte_len
  }

  #[inline]
  pub fn alloc_len(&self) -> usize {
    self.alloc_len
  }

  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    self.ptr.as_ptr()
  }

  #[inline]
  pub fn is_pinned(&self) -> bool {
    let Some(ctl) = NonNull::new(self.ctl) else {
      return false;
    };
    // SAFETY: ctl is valid while the backing store is alive; freeing while pinned is delayed.
    unsafe { ctl.as_ref() }.pin_count.load(Ordering::Acquire) > 0
  }

  pub fn pin(&self) -> Result<(PinnedBackingStore, (*mut u8, usize)), BackingStorePinError> {
    self.pin_range(0..self.byte_len)
  }

  pub fn pin_range(
    &self,
    range: core::ops::Range<usize>,
  ) -> Result<(PinnedBackingStore, (*mut u8, usize)), BackingStorePinError> {
    if range.start > range.end || range.end > self.byte_len {
      return Err(BackingStorePinError::OutOfBounds);
    }
    let ctl = NonNull::new(self.ctl).ok_or(BackingStorePinError::NotAlive)?;

    // SAFETY: ctl points at the stable pin/state controller for this backing store.
    let ctl_ref = unsafe { ctl.as_ref() };

    if ctl_ref.state() != BackingStoreState::Alive {
      return Err(BackingStorePinError::NotAlive);
    }

    ctl_ref.pin_count.fetch_add(1, Ordering::AcqRel);
    let pinned = PinnedBackingStore { ctl };

    // Re-check after increment to avoid racing with detach/free.
    if ctl_ref.state() != BackingStoreState::Alive {
      drop(pinned);
      return Err(BackingStorePinError::NotAlive);
    }

    let out_ptr = unsafe { self.as_ptr().add(range.start) };
    Ok((pinned, (out_ptr, range.end - range.start)))
  }

  pub fn try_detach(&mut self) -> Result<(), BackingStoreDetachError> {
    self.try_invalidate(BackingStoreState::Detaching, false)
  }

  pub fn try_free(&mut self) -> Result<(), BackingStoreDetachError> {
    self.try_invalidate(BackingStoreState::Freeing, false)
  }

  pub(crate) fn free_or_mark_pending(&mut self) {
    let _ = self.try_invalidate(BackingStoreState::Freeing, true);
  }

  #[inline]
  fn is_ptr_min_aligned(ptr: *const u8) -> bool {
    (ptr as usize) % BACKING_STORE_MIN_ALIGN == 0
  }

  /// Creates a `BackingStore` from an existing allocation.
  ///
  /// # Safety
  /// Caller must uphold the struct invariants documented on [`BackingStore`].
  #[inline]
  unsafe fn from_raw_parts(
    ptr: *mut u8,
    byte_len: usize,
    alloc_len: usize,
    alloc_align: usize,
    external_bytes: *const AtomicUsize,
  ) -> Self {
    debug_assert!(
      alloc_len == 0 || ptr != core::ptr::null_mut(),
      "non-empty BackingStore must not have a null ptr"
    );
    debug_assert!(
      alloc_align.is_power_of_two(),
      "alloc_align must be a power of two"
    );
    debug_assert!(
      alloc_len == 0 || (ptr as usize) % BACKING_STORE_MIN_ALIGN == 0,
      "BackingStore pointer must be at least {BACKING_STORE_MIN_ALIGN}-byte aligned"
    );

    let ctl = if alloc_len == 0 {
      core::ptr::null_mut()
    } else {
      Box::into_raw(Box::new(BackingStoreCtl {
        state: AtomicU8::new(BackingStoreState::Alive as u8),
        pin_count: AtomicUsize::new(0),
        alloc_ptr: ptr,
        alloc_len,
        alloc_align,
        external_bytes,
      }))
    };

    Self {
      ptr: NonNull::new_unchecked(if alloc_len == 0 {
        NonNull::dangling().as_ptr()
      } else {
        ptr
      }),
      byte_len,
      alloc_len,
      alloc_align,
      ctl,
    }
  }

  fn try_invalidate(
    &mut self,
    in_progress: BackingStoreState,
    allow_pending_free: bool,
  ) -> Result<(), BackingStoreDetachError> {
    let ctl = NonNull::new(self.ctl).ok_or(BackingStoreDetachError::NotAlive)?;
    // SAFETY: ctl is valid while the store is alive.
    let ctl_ref = unsafe { ctl.as_ref() };

    // Block new pins before checking pin_count.
    if ctl_ref
      .state
      .compare_exchange(
        BackingStoreState::Alive as u8,
        in_progress as u8,
        Ordering::AcqRel,
        Ordering::Acquire,
      )
      .is_err()
    {
      return Err(BackingStoreDetachError::NotAlive);
    }

    if ctl_ref.pin_count.load(Ordering::Acquire) != 0 {
      if allow_pending_free && in_progress == BackingStoreState::Freeing {
        ctl_ref
          .state
          .store(BackingStoreState::FreePending as u8, Ordering::Release);
        // Detach the header's handle; the last pin drop will perform the actual free.
        *self = BackingStore::empty();
        return Ok(());
      }

      ctl_ref
        .state
        .store(BackingStoreState::Alive as u8, Ordering::Release);
      return Err(BackingStoreDetachError::Pinned);
    }

    // Unpinned: free immediately.
    unsafe { finalize_ctl(ctl) };
    *self = BackingStore::empty();
    Ok(())
  }
}

/// Allocator abstraction for `BackingStore` memory.
///
/// This exists because kernel I/O often requires a stable address under a moving GC. The backing
/// store is therefore allocated *outside* the GC heap using a non-moving allocator.
pub trait BackingStoreAllocator {
  fn alloc_zeroed(&self, len: usize) -> Result<BackingStore, BackingStoreAllocError>;

  fn alloc_uninit(&self, len: usize) -> Result<BackingStore, BackingStoreAllocError>;

  /// Adopts an existing `Vec<u8>` allocation without copying when possible.
  ///
  /// If the allocation pointer is not aligned to [`BACKING_STORE_MIN_ALIGN`], this method may fall
  /// back to allocating a fresh backing store and copying the bytes.
  fn adopt_vec(&self, bytes: Vec<u8>) -> Result<BackingStore, BackingStoreAllocError>;

  /// Adopts an existing `Box<[u8]>` allocation without copying when possible.
  ///
  /// If the allocation pointer is not aligned to [`BACKING_STORE_MIN_ALIGN`], this method may fall
  /// back to allocating a fresh backing store and copying the bytes.
  fn adopt_boxed_slice(&self, bytes: Box<[u8]>) -> Result<BackingStore, BackingStoreAllocError>;

  /// Releases the backing store, decrementing external memory accounting.
  ///
  /// This method is intended to be called by the GC finalizer for the corresponding `ArrayBuffer`
  /// header object.
  fn free(&self, store: &mut BackingStore);

  /// Number of bytes currently owned by this allocator that live outside the GC heap.
  fn external_bytes(&self) -> usize;
}

/// Backing store allocator using Rust's global allocator (`alloc`/`dealloc`).
///
/// All allocations are at least [`BACKING_STORE_MIN_ALIGN`]-aligned.
#[derive(Debug, Default)]
pub struct GlobalBackingStoreAllocator {
  external_bytes: AtomicUsize,
}

impl GlobalBackingStoreAllocator {
  pub const fn new() -> Self {
    Self {
      external_bytes: AtomicUsize::new(0),
    }
  }

  #[inline]
  fn bump_external(&self, bytes: usize) {
    if bytes == 0 {
      return;
    }
    self.external_bytes.fetch_add(bytes, Ordering::Relaxed);
  }

  #[inline]
  fn dec_external(&self, bytes: usize) {
    if bytes == 0 {
      return;
    }
    let prev = self.external_bytes.fetch_sub(bytes, Ordering::Relaxed);
    debug_assert!(
      prev >= bytes,
      "external_bytes underflow (prev={prev}, sub={bytes})"
    );
  }

  fn alloc_raw(&self, len: usize, zeroed: bool) -> Result<BackingStore, BackingStoreAllocError> {
    if len == 0 {
      return Ok(BackingStore::empty());
    }

    let layout = Layout::from_size_align(len, BACKING_STORE_MIN_ALIGN)
      .map_err(|_| BackingStoreAllocError::InvalidLayout)?;
    let ptr = unsafe {
      if zeroed {
        alloc_zeroed(layout)
      } else {
        alloc(layout)
      }
    };
    let Some(ptr) = NonNull::new(ptr) else {
      return Err(BackingStoreAllocError::OutOfMemory);
    };

    debug_assert!(BackingStore::is_ptr_min_aligned(ptr.as_ptr()));
    self.bump_external(len);
    // SAFETY: `ptr` is non-null, points to `len` bytes allocated with `layout.align() == BACKING_STORE_MIN_ALIGN`.
    Ok(unsafe {
      BackingStore::from_raw_parts(
        ptr.as_ptr(),
        len,
        len,
        BACKING_STORE_MIN_ALIGN,
        &self.external_bytes,
      )
    })
  }

  fn adopt_or_copy(
    &self,
    ptr: *mut u8,
    byte_len: usize,
    alloc_len: usize,
  ) -> Result<BackingStore, BackingStoreAllocError> {
    debug_assert!(byte_len <= alloc_len);

    if byte_len == 0 {
      // Nothing to keep; caller should have already freed any allocation.
      return Ok(BackingStore::empty());
    }

    if BackingStore::is_ptr_min_aligned(ptr) {
      self.bump_external(alloc_len);
      // SAFETY: caller guarantees `ptr` points to `alloc_len` bytes with alignment 1, and we've
      // checked the address meets our min alignment requirement.
      return Ok(unsafe {
        BackingStore::from_raw_parts(ptr, byte_len, alloc_len, 1, &self.external_bytes)
      });
    }

    // Misaligned: allocate an aligned buffer and copy.
    let store = self.alloc_uninit(byte_len)?;
    unsafe {
      core::ptr::copy_nonoverlapping(ptr, store.as_ptr(), byte_len);
    }
    Ok(store)
  }

  fn free_raw_allocation(&self, ptr: *mut u8, alloc_len: usize, alloc_align: usize) {
    if alloc_len == 0 {
      return;
    }
    // We only ever allocate with (1) alignment 1 via Vec/Box or (2) alignment BACKING_STORE_MIN_ALIGN.
    debug_assert!(alloc_align == 1 || alloc_align == BACKING_STORE_MIN_ALIGN);

    // Layout construction should not fail for previously-allocated layouts.
    let layout = Layout::from_size_align(alloc_len, alloc_align)
      .expect("valid layout for backing store dealloc");
    unsafe {
      dealloc(ptr, layout);
    }
  }
}

impl BackingStoreAllocator for GlobalBackingStoreAllocator {
  fn alloc_zeroed(&self, len: usize) -> Result<BackingStore, BackingStoreAllocError> {
    self.alloc_raw(len, true)
  }

  fn alloc_uninit(&self, len: usize) -> Result<BackingStore, BackingStoreAllocError> {
    self.alloc_raw(len, false)
  }

  fn adopt_vec(&self, bytes: Vec<u8>) -> Result<BackingStore, BackingStoreAllocError> {
    let mut bytes = ManuallyDrop::new(bytes);
    let ptr = bytes.as_mut_ptr();
    let byte_len = bytes.len();
    let alloc_len = bytes.capacity();
    if alloc_len == 0 {
      return Ok(BackingStore::empty());
    }
    if byte_len == 0 {
      // The caller provided an empty Vec that still owns an allocation. Since an empty `ArrayBuffer`
      // has no bytes to adopt, immediately release it.
      self.free_raw_allocation(ptr, alloc_len, 1);
      return Ok(BackingStore::empty());
    }

    let store = self.adopt_or_copy(ptr, byte_len, alloc_len);
    if store.is_err() || !BackingStore::is_ptr_min_aligned(ptr) {
      // We either failed to allocate the replacement, or we allocated a replacement due to
      // misalignment. In both cases we must free the original `Vec` allocation.
      self.free_raw_allocation(ptr, alloc_len, 1);
    }
    store
  }

  fn adopt_boxed_slice(&self, bytes: Box<[u8]>) -> Result<BackingStore, BackingStoreAllocError> {
    let byte_len = bytes.len();
    if byte_len == 0 {
      return Ok(BackingStore::empty());
    }

    let ptr = Box::into_raw(bytes) as *mut u8;

    let store = self.adopt_or_copy(ptr, byte_len, byte_len);
    if store.is_err() || !BackingStore::is_ptr_min_aligned(ptr) {
      // We either failed to allocate the replacement, or we allocated a replacement due to
      // misalignment. In both cases we must free the original `Box` allocation.
      self.free_raw_allocation(ptr, byte_len, 1);
    }
    store
  }

  fn free(&self, store: &mut BackingStore) {
    store.free_or_mark_pending();
  }

  fn external_bytes(&self) -> usize {
    self.external_bytes.load(Ordering::Relaxed)
  }
}

static GLOBAL_BACKING_STORE_ALLOCATOR: GlobalBackingStoreAllocator =
  GlobalBackingStoreAllocator::new();

/// Global backing store allocator.
///
/// This is intended as a stopgap for the early runtime; real embeddings can construct their own
/// allocator instance and plumb it through runtime state.
#[inline]
pub fn global_backing_store_allocator() -> &'static GlobalBackingStoreAllocator {
  &GLOBAL_BACKING_STORE_ALLOCATOR
}

