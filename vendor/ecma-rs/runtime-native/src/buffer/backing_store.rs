use core::alloc::Layout;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
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
}

impl BackingStore {
  #[inline]
  pub fn empty() -> Self {
    Self {
      ptr: NonNull::dangling(),
      byte_len: 0,
      alloc_len: 0,
      alloc_align: BACKING_STORE_MIN_ALIGN,
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

    Self {
      ptr: NonNull::new_unchecked(if alloc_len == 0 {
        NonNull::dangling().as_ptr()
      } else {
        ptr
      }),
      byte_len,
      alloc_len,
      alloc_align,
    }
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
    Ok(unsafe { BackingStore::from_raw_parts(ptr.as_ptr(), len, len, BACKING_STORE_MIN_ALIGN) })
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
      return Ok(unsafe { BackingStore::from_raw_parts(ptr, byte_len, alloc_len, 1) });
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
    if store.alloc_len == 0 {
      return;
    }

    let ptr = store.ptr.as_ptr();
    let alloc_len = store.alloc_len;
    let alloc_align = store.alloc_align;

    self.free_raw_allocation(ptr, alloc_len, alloc_align);
    self.dec_external(alloc_len);

    *store = BackingStore::empty();
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
