use core::alloc::Layout;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::alloc::{alloc, alloc_zeroed, dealloc};
use std::sync::{Arc, OnceLock};

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
  /// The backing store has already been detached/finalized.
  NotAlive,
  /// Requested pin range exceeded the visible `byte_len`.
  OutOfBounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingStoreDetachError {
  /// Backing store is currently pinned (in-flight).
  Pinned,
  /// Backing store has already been detached/finalized.
  NotAlive,
}

#[derive(Debug)]
struct BackingStoreInner {
  ptr: NonNull<u8>,
  /// Visible length in bytes (ArrayBuffer `byteLength`).
  byte_len: usize,
  /// Actual allocation size in bytes. For adopted `Vec<u8>` this may be `capacity`.
  alloc_len: usize,
  /// Allocation alignment used for deallocation. For adopted `Vec<u8>` / `Box<[u8]>` this is 1.
  alloc_align: usize,

  /// Tracks externally allocated bytes.
  ///
  /// This is incremented once on allocation and decremented on actual free (when the last strong
  /// `BackingStore` reference is dropped).
  external_bytes: Arc<AtomicUsize>,

  /// Number of active pin guards.
  pin_count: AtomicU32,
}

impl Drop for BackingStoreInner {
  fn drop(&mut self) {
    debug_assert_eq!(
      self.pin_count.load(Ordering::Acquire),
      0,
      "backing store dropped while pinned"
    );

    if self.alloc_len == 0 {
      return;
    }

    let layout = Layout::from_size_align(self.alloc_len, self.alloc_align)
      .expect("valid layout for backing store dealloc");
    unsafe {
      dealloc(self.ptr.as_ptr(), layout);
    }

    let prev = self.external_bytes.fetch_sub(self.alloc_len, Ordering::Relaxed);
    debug_assert!(
      prev >= self.alloc_len,
      "external_bytes underflow (prev={prev}, sub={})",
      self.alloc_len
    );
  }
}

/// A stable, non-moving byte buffer used as the backing store for `ArrayBuffer`/`TypedArray`.
///
/// This is an independently-owned object, separate from any GC-managed `ArrayBuffer` header. It is
/// reference-counted so host pin guards can keep the allocation alive even if the owning header is
/// collected by the GC.
///
/// The allocation is freed and external-bytes accounting is decremented only when the last
/// [`BackingStore`] handle is dropped.
#[derive(Clone, Debug)]
pub struct BackingStore {
  inner: Arc<BackingStoreInner>,
}

impl BackingStore {
  #[inline]
  pub fn byte_len(&self) -> usize {
    self.inner.byte_len
  }

  #[inline]
  pub fn alloc_len(&self) -> usize {
    self.inner.alloc_len
  }

  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    self.inner.ptr.as_ptr()
  }

  #[inline]
  pub fn pin_count(&self) -> u32 {
    self.inner.pin_count.load(Ordering::Acquire)
  }

  #[inline]
  pub fn is_pinned(&self) -> bool {
    self.pin_count() != 0
  }

  pub fn pin_guard(&self) -> PinnedBackingStore {
    self.inner.pin_count.fetch_add(1, Ordering::AcqRel);
    PinnedBackingStore { store: self.clone() }
  }

  pub fn pin(&self) -> Result<(PinnedBackingStore, (*mut u8, usize)), BackingStorePinError> {
    self.pin_range(0..self.byte_len())
  }

  pub fn pin_range(
    &self,
    range: core::ops::Range<usize>,
  ) -> Result<(PinnedBackingStore, (*mut u8, usize)), BackingStorePinError> {
    if range.start > range.end || range.end > self.byte_len() {
      return Err(BackingStorePinError::OutOfBounds);
    }

    let pinned = self.pin_guard();
    // SAFETY: bounds checked above.
    let ptr = unsafe { pinned.as_ptr().add(range.start) };
    Ok((pinned, (ptr, range.end - range.start)))
  }

  #[inline]
  fn is_ptr_min_aligned(ptr: *const u8) -> bool {
    (ptr as usize) % BACKING_STORE_MIN_ALIGN == 0
  }

  #[inline]
  fn empty(external_bytes: Arc<AtomicUsize>) -> Self {
    // SAFETY: `alloc_len == 0` implies `ptr` is never dereferenced.
    unsafe { Self::from_raw_parts(core::ptr::null_mut(), 0, 0, BACKING_STORE_MIN_ALIGN, external_bytes) }
  }

  /// Creates a `BackingStore` from an existing allocation.
  ///
  /// # Safety
  /// Caller must uphold the invariants documented on [`BackingStoreInner`].
  #[inline]
  unsafe fn from_raw_parts(
    ptr: *mut u8,
    byte_len: usize,
    alloc_len: usize,
    alloc_align: usize,
    external_bytes: Arc<AtomicUsize>,
  ) -> Self {
    debug_assert!(byte_len <= alloc_len);
    debug_assert!(
      alloc_len == 0 || ptr != core::ptr::null_mut(),
      "non-empty BackingStore must not have a null ptr"
    );
    debug_assert!(
      alloc_align.is_power_of_two(),
      "alloc_align must be a power of two"
    );
    debug_assert!(
      alloc_len == 0 || Self::is_ptr_min_aligned(ptr),
      "BackingStore pointer must be at least {BACKING_STORE_MIN_ALIGN}-byte aligned"
    );

    if alloc_len != 0 {
      external_bytes.fetch_add(alloc_len, Ordering::Relaxed);
    }

    Self {
      inner: Arc::new(BackingStoreInner {
        ptr: NonNull::new_unchecked(if alloc_len == 0 {
          NonNull::dangling().as_ptr()
        } else {
          ptr
        }),
        byte_len,
        alloc_len,
        alloc_align,
        external_bytes,
        pin_count: AtomicU32::new(0),
      }),
    }
  }
}

/// A host-visible pin guard for a backing store allocation.
///
/// Holding this keeps the backing store alive (strong reference) and prevents detachment while the
/// host is doing I/O/FFI using the raw pointer.
#[derive(Debug)]
pub struct PinnedBackingStore {
  store: BackingStore,
}

impl PinnedBackingStore {
  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    self.store.as_ptr()
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.store.byte_len()
  }
}

impl Drop for PinnedBackingStore {
  fn drop(&mut self) {
    let prev = self.store.inner.pin_count.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(prev > 0, "backing store pin_count underflow");
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

  /// Number of bytes currently owned by this allocator that live outside the GC heap.
  fn external_bytes(&self) -> usize;
}

/// Backing store allocator using Rust's global allocator (`alloc`/`dealloc`).
///
/// All allocations are at least [`BACKING_STORE_MIN_ALIGN`]-aligned.
#[derive(Debug, Clone)]
pub struct GlobalBackingStoreAllocator {
  external_bytes: Arc<AtomicUsize>,
}

impl Default for GlobalBackingStoreAllocator {
  fn default() -> Self {
    Self {
      external_bytes: Arc::new(AtomicUsize::new(0)),
    }
  }
}

impl GlobalBackingStoreAllocator {
  fn alloc_raw(&self, len: usize, zeroed: bool) -> Result<BackingStore, BackingStoreAllocError> {
    if len == 0 {
      return Ok(BackingStore::empty(Arc::clone(&self.external_bytes)));
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
    Ok(unsafe {
      BackingStore::from_raw_parts(
        ptr.as_ptr(),
        len,
        len,
        BACKING_STORE_MIN_ALIGN,
        Arc::clone(&self.external_bytes),
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
      return Ok(BackingStore::empty(Arc::clone(&self.external_bytes)));
    }

    if BackingStore::is_ptr_min_aligned(ptr) {
      // SAFETY: caller guarantees `ptr` points to `alloc_len` bytes with alignment 1, and we've
      // checked the address meets our min alignment requirement.
      return Ok(unsafe {
        BackingStore::from_raw_parts(
          ptr,
          byte_len,
          alloc_len,
          1,
          Arc::clone(&self.external_bytes),
        )
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
    debug_assert!(alloc_align == 1 || alloc_align == BACKING_STORE_MIN_ALIGN);

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
      return Ok(BackingStore::empty(Arc::clone(&self.external_bytes)));
    }
    if byte_len == 0 {
      // The caller provided an empty Vec that still owns an allocation. Since an empty `ArrayBuffer`
      // has no bytes to adopt, immediately release it.
      self.free_raw_allocation(ptr, alloc_len, 1);
      return Ok(BackingStore::empty(Arc::clone(&self.external_bytes)));
    }

    let store = self.adopt_or_copy(ptr, byte_len, alloc_len);
    if store.is_err() || !BackingStore::is_ptr_min_aligned(ptr) {
      self.free_raw_allocation(ptr, alloc_len, 1);
    }
    store
  }

  fn adopt_boxed_slice(&self, bytes: Box<[u8]>) -> Result<BackingStore, BackingStoreAllocError> {
    let byte_len = bytes.len();
    if byte_len == 0 {
      return Ok(BackingStore::empty(Arc::clone(&self.external_bytes)));
    }

    let ptr = Box::into_raw(bytes) as *mut u8;

    let store = self.adopt_or_copy(ptr, byte_len, byte_len);
    if store.is_err() || !BackingStore::is_ptr_min_aligned(ptr) {
      self.free_raw_allocation(ptr, byte_len, 1);
    }
    store
  }

  fn external_bytes(&self) -> usize {
    self.external_bytes.load(Ordering::Relaxed)
  }
}

static GLOBAL_BACKING_STORE_ALLOCATOR: OnceLock<GlobalBackingStoreAllocator> = OnceLock::new();

/// Global backing store allocator.
///
/// This is intended as a stopgap for the early runtime; real embeddings can construct their own
/// allocator instance and plumb it through runtime state.
#[inline]
pub fn global_backing_store_allocator() -> &'static GlobalBackingStoreAllocator {
  GLOBAL_BACKING_STORE_ALLOCATOR.get_or_init(GlobalBackingStoreAllocator::default)
}
