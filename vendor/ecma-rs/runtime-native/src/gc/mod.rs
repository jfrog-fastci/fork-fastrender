use std::mem;
use std::slice;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static GC_IN_PROGRESS_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Returns `true` while a GC cycle (minor or major) is actively running.
pub(crate) fn gc_in_progress() -> bool {
  GC_IN_PROGRESS_DEPTH.load(Ordering::Acquire) != 0
}

/// RAII guard that marks a GC cycle as active for the duration of its lifetime.
///
/// This is used to assert that runtime root management (shadow stack push/pop) does not occur while
/// the GC is actively tracing/evacuating.
#[must_use]
pub(crate) struct GcInProgressGuard(());

impl GcInProgressGuard {
  pub(crate) fn new() -> Self {
    GC_IN_PROGRESS_DEPTH.fetch_add(1, Ordering::SeqCst);
    Self(())
  }
}

impl Drop for GcInProgressGuard {
  fn drop(&mut self) {
    GC_IN_PROGRESS_DEPTH.fetch_sub(1, Ordering::SeqCst);
  }
}

use crate::array;
use crate::trap;

pub mod config;
pub mod global_remset;
pub mod card_table;
pub mod write_barrier;
pub mod heap;
pub mod roots;
pub mod handle_table;
pub mod shadow_stack;
pub mod thread;
pub mod weak;
mod young;
mod cards;

mod evacuate;
mod mark;
mod keep_alive;
mod work_stack;
pub use config::{HeapConfig, HeapLimits};
pub use heap::{AllocError, AllocKind, AllocRequest, GcHeap, PersistentRoot};
pub use keep_alive::keep_alive_gc_ref;
pub use handle_table::{HandleId, HandleTable, OwnedGcHandle, PersistentHandle};
pub use roots::RememberedSet;
pub use roots::RootHandle;
pub use roots::RootSet;
pub use roots::RootStack;
pub use shadow_stack::RootScope;
pub use shadow_stack::ShadowStack;
pub use roots::SimpleRememberedSet;
pub use thread::with_thread_state;
pub use weak::register_weak_cleanup;
pub use weak::WeakHandle;
pub use weak::WeakHandles;
pub use young::YoungSpace;

pub(crate) use young::YOUNG_SPACE;
#[cfg(any(debug_assertions, feature = "gc_debug"))]
pub(crate) use verify::register_type_descriptor_ptr;

/// Align `value` up to the next multiple of `align` (power-of-two).
///
/// This is shared by a few fixed-size object layout helpers (e.g. the string
/// interner) that need a stable, aligned `TypeDescriptor::size`.
#[inline]
pub(crate) fn align_up(value: usize, align: usize) -> usize {
  if align == 0 || !align.is_power_of_two() {
    trap::rt_trap_invalid_arg("align_up: align must be a non-zero power of two");
  }
  debug_assert!(align.is_power_of_two());
  let mask = align - 1;
  value
    .checked_add(mask)
    .map(|v| v & !mask)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("align_up overflow"))
}

/// Number of bytes covered by a single card in a per-object card table.
pub const CARD_SIZE: usize = 512;

/// Minimum array payload size (in bytes) to enable a per-object card table.
///
/// Card tables are only installed for **old-generation pointer arrays** whose
/// element payload is at least this large. For smaller arrays, rescanning the
/// full object is typically cheaper than maintaining card metadata.
///
/// See `docs/write_barrier.md` for the benchmark-driven default heuristic.
pub const CARD_TABLE_MIN_BYTES: usize = 8 * CARD_SIZE;

/// Return the number of `AtomicU64` words required for a per-object card table
/// covering an object of `obj_size` bytes.
///
/// Card table bitset layout:
/// - `cards = ceil(obj_size / CARD_SIZE)` bits
/// - `words = ceil(cards / 64)` 64-bit words
#[inline]
pub(crate) fn card_table_word_count(obj_size: usize) -> usize {
  if obj_size == 0 {
    return 0;
  }
  let card_count = obj_size.div_ceil(CARD_SIZE);
  card_count.div_ceil(64)
}

/// Clear the card table bitset for `obj` if one is installed.
///
/// # Safety
/// `obj` must be a valid GC object base pointer.
pub(crate) unsafe fn clear_card_table_for_obj(obj: *mut u8) {
  debug_assert!(!obj.is_null());
  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if card_table.is_null() {
    return;
  }

  let size = obj_size(obj);
  let words = card_table_word_count(size);
  for i in 0..words {
    (*card_table.add(i)).store(0, Ordering::Release);
  }
}
/// Object header that prefixes every GC-managed allocation.
///
/// # Layout
/// The object pointer is a raw `*mut u8` that points at the start of this header.
/// The header is followed by the object's payload as described by its
/// [`TypeDescriptor`].
#[repr(C)]
pub struct ObjHeader {
  pub(crate) type_desc: *const TypeDescriptor,
  pub(crate) meta: AtomicUsize,
}

// SAFETY: `ObjHeader` contains only an immutable type descriptor pointer (global, read-only data)
// plus atomic metadata. It is safe to move and share between threads.
unsafe impl Send for ObjHeader {}
unsafe impl Sync for ObjHeader {}

pub const OBJ_HEADER_SIZE: usize = mem::size_of::<ObjHeader>();
/// Minimum alignment (in bytes) guaranteed for all GC-managed object base pointers.
///
/// Codegen may assume `rt_alloc` / `rt_alloc_pinned` return pointers aligned to at least this.
pub const OBJ_ALIGN: usize = if mem::align_of::<ObjHeader>() > 16 {
  mem::align_of::<ObjHeader>()
} else {
  16
};
// `meta` layout:
// - bit 0: forwarded bit (nursery only)
// - bit 1: mark epoch (0/1)
// - bit 2: remembered bit (old object has an old->young pointer)
// - bit 3: pinned bit (object must not be moved by compaction/evacuation)
const META_FORWARDED: usize = 1;
const META_MARK_SHIFT: usize = 1;
const META_MARK_MASK: usize = 1 << META_MARK_SHIFT;
const META_REMEMBERED: usize = 1 << 2;
const META_PINNED: usize = 1 << 3;
const META_FLAGS_MASK: usize = META_FORWARDED | META_MARK_MASK | META_REMEMBERED | META_PINNED;
/// Alignment required of card table pointers stored in [`ObjHeader::meta`].
///
/// Card table pointers are stored in the high bits of `meta`; the low [`META_FLAGS_MASK`] bits are
/// reserved for flags. Requiring `ptr & META_FLAGS_MASK == 0` is equivalent to requiring
/// `ptr` be aligned to `META_FLAGS_MASK + 1` bytes.
pub(crate) const CARD_TABLE_PTR_ALIGN: usize = META_FLAGS_MASK + 1;
const _: () = assert!(CARD_TABLE_PTR_ALIGN.is_power_of_two());

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
static CARD_TABLE_BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
static CARD_TABLE_BYTES_FREED: AtomicU64 = AtomicU64::new(0);

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
#[doc(hidden)]
pub fn card_table_bytes_allocated_for_tests() -> u64 {
  CARD_TABLE_BYTES_ALLOCATED.load(Ordering::Relaxed)
}

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
#[doc(hidden)]
pub fn card_table_bytes_freed_for_tests() -> u64 {
  CARD_TABLE_BYTES_FREED.load(Ordering::Relaxed)
}

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
#[doc(hidden)]
pub fn reset_card_table_counters_for_tests() {
  CARD_TABLE_BYTES_ALLOCATED.store(0, Ordering::Relaxed);
  CARD_TABLE_BYTES_FREED.store(0, Ordering::Relaxed);
}

#[cfg(unix)]
#[inline]
fn page_size() -> usize {
  // SAFETY: sysconf is thread-safe.
  let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if ps <= 0 { 4096 } else { ps as usize }
}

#[cfg(unix)]
#[inline]
fn round_up_to_page_size(bytes: usize) -> usize {
  align_up(bytes, page_size())
}

#[inline]
fn card_table_alloc_bytes_unrounded(obj_size: usize) -> usize {
  let word_count = card_table_word_count(obj_size);
  if word_count == 0 {
    return 0;
  }
  word_count
    .checked_mul(mem::size_of::<AtomicU64>())
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("card table size overflow"))
}

#[inline]
fn card_table_alloc_bytes(obj_size: usize) -> usize {
  let bytes = card_table_alloc_bytes_unrounded(obj_size);
  if bytes == 0 {
    return 0;
  }
  #[cfg(unix)]
  {
    return round_up_to_page_size(bytes);
  }
  #[cfg(not(unix))]
  {
    bytes
  }
}

#[inline]
pub(crate) fn alloc_card_table(obj_size: usize) -> *mut AtomicU64 {
  let word_count = card_table_word_count(obj_size);
  if word_count == 0 {
    return core::ptr::null_mut();
  }
  let bytes = card_table_alloc_bytes(obj_size);

  // Card tables are reclaimed when their owning objects are swept (major GC and
  // LOS sweep). We use `mmap` on Unix so installing card tables during GC (e.g.
  // promotion) does not rely on the Rust global allocator.
  #[cfg(unix)]
  let raw = unsafe {
    loop {
      let raw = libc::mmap(
        core::ptr::null_mut(),
        bytes,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANON,
        -1,
        0,
      );
      if raw == libc::MAP_FAILED {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
          continue;
        }
        break raw;
      }
      if raw.is_null() {
        // Mapping at address 0 is unexpected; unmap and treat as OOM.
        let _ = libc::munmap(raw, bytes);
        break raw;
      }
      break raw;
    }
  };
  #[cfg(unix)]
  let ptr = {
    if raw == libc::MAP_FAILED || raw.is_null() {
      trap::rt_trap_oom(bytes, "card table");
    }
    raw as *mut AtomicU64
  };

  #[cfg(not(unix))]
  let ptr = {
    let align = CARD_TABLE_PTR_ALIGN.max(mem::align_of::<AtomicU64>());
    let layout = std::alloc::Layout::from_size_align(bytes, align)
      .unwrap_or_else(|_| trap::rt_trap_invalid_arg("invalid card table layout"));
    // SAFETY: layout is non-zero and well-formed.
    let raw = unsafe { std::alloc::alloc_zeroed(layout) };
    if raw.is_null() {
      trap::rt_trap_oom(bytes, "card table");
    }
    raw as *mut AtomicU64
  };

  debug_assert!(
    (ptr as usize & META_FLAGS_MASK) == 0,
    "card table pointer must satisfy ObjHeader::set_card_table_ptr alignment constraint"
  );

  // Initialize the atomics (even though the backing pages are zeroed) so the
  // values are fully-formed `AtomicU64`s.
  for i in 0..word_count {
    // SAFETY: `ptr` points to `word_count` `AtomicU64` slots.
    unsafe {
      ptr.add(i).write(AtomicU64::new(0));
    }
  }

  #[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
  CARD_TABLE_BYTES_ALLOCATED.fetch_add(bytes as u64, Ordering::Relaxed);

  ptr
}

/// Free a per-object card table previously allocated by [`alloc_card_table`].
///
/// # Safety
/// - `card_table_ptr` must be null or a pointer previously returned by
///   `alloc_card_table(obj_size)` for the given `obj_size`.
/// - The caller must ensure the card table is not concurrently accessed.
pub(crate) unsafe fn free_card_table(card_table_ptr: *mut AtomicU64, obj_size: usize) {
  if card_table_ptr.is_null() {
    return;
  }
  let bytes = card_table_alloc_bytes(obj_size);
  if bytes == 0 {
    return;
  }

  #[cfg(unix)]
  {
    loop {
      let rc = libc::munmap(card_table_ptr.cast(), bytes);
      if rc == 0 {
        break;
      }
      let err = std::io::Error::last_os_error();
      if err.kind() == std::io::ErrorKind::Interrupted {
        continue;
      }
      // This is a runtime bug (wrong pointer/length). Avoid unwinding across
      // arbitrary runtime code and fail fast.
      if cfg!(debug_assertions) {
        eprintln!("runtime-native: munmap(card table) failed: {err}");
      }
      std::process::abort();
    }
  }

  #[cfg(not(unix))]
  {
    let bytes_unrounded = card_table_alloc_bytes_unrounded(obj_size);
    if bytes_unrounded == 0 {
      return;
    }
    let align = CARD_TABLE_PTR_ALIGN.max(mem::align_of::<AtomicU64>());
    let layout = std::alloc::Layout::from_size_align(bytes_unrounded, align)
      .unwrap_or_else(|_| trap::rt_trap_invalid_arg("invalid card table layout"));
    std::alloc::dealloc(card_table_ptr.cast(), layout);
  }

  #[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
  CARD_TABLE_BYTES_FREED.fetch_add(bytes as u64, Ordering::Relaxed);
}

impl ObjHeader {
  pub const fn new(type_desc: &'static TypeDescriptor) -> Self {
    Self {
      type_desc: type_desc as *const TypeDescriptor,
      meta: AtomicUsize::new(0),
    }
  }

  #[inline]
  pub(crate) unsafe fn type_desc(&self) -> &TypeDescriptor {
    debug_assert!(!self.type_desc.is_null());
    &*self.type_desc
  }

  #[inline]
  pub(crate) fn is_forwarded(&self) -> bool {
    (self.meta.load(Ordering::Acquire) & META_FORWARDED) != 0
  }

  #[inline]
  pub(crate) fn forwarding_ptr(&self) -> *mut u8 {
    debug_assert!(self.is_forwarded());
    (self.meta.load(Ordering::Acquire) & !META_FORWARDED) as *mut u8
  }

  #[inline]
  pub(crate) fn set_forwarding_ptr(&mut self, new_location: *mut u8) {
    debug_assert!((new_location as usize & META_FORWARDED) == 0);
    assert!(
      !self.is_pinned(),
      "attempted to evacuate/forward a pinned object"
    );
    self
      .meta
      .store((new_location as usize) | META_FORWARDED, Ordering::Release);
  }

  #[inline]
  pub fn is_remembered(&self) -> bool {
    // When the header is in the "forwarded" state, `meta` is a tagged forwarding pointer, so any
    // other bit tests are meaningless.
    !self.is_forwarded() && (self.meta.load(Ordering::Acquire) & META_REMEMBERED) != 0
  }

  /// Debug/test helper: clear the per-object `REMEMBERED` bit.
  ///
  /// This is intentionally not part of the stable runtime ABI; it exists so
  /// integration tests can model remembered-set rebuild behavior without
  /// depending on a process-global remembered set.
  #[doc(hidden)]
  pub fn clear_remembered_for_tests(&self) {
    if self.is_forwarded() {
      return;
    }
    self.meta.fetch_and(!META_REMEMBERED, Ordering::Relaxed);
  }

  #[inline]
  pub fn is_pinned(&self) -> bool {
    // When the header is in the "forwarded" state, `meta` is a tagged forwarding pointer, so any
    // other bit tests are meaningless.
    !self.is_forwarded() && (self.meta.load(Ordering::Acquire) & META_PINNED) != 0
  }

  /// Atomically set the remembered bit if it is currently unset.
  ///
  /// Returns `true` if this call transitioned `REMEMBERED` from 0 → 1.
  #[inline]
  pub(crate) fn set_remembered_idempotent(&self) -> bool {
    // Forwarded objects store relocation pointers in `meta` (used during minor
    // GC and optional major compaction). Remembered bits are not meaningful on
    // forwarded from-space objects, and setting bits would corrupt the pointer.
    if self.is_forwarded() {
      return false;
    }
    let prev = self.meta.fetch_or(META_REMEMBERED, Ordering::Relaxed);
    (prev & META_REMEMBERED) == 0
  }

  /// Alias for [`ObjHeader::set_remembered_idempotent`].
  ///
  /// This exists for generational write barrier code and tests that prefer a
  /// name describing the 0→1 transition.
  #[inline]
  #[doc(hidden)]
  pub fn set_remembered_if_unset(&self) -> bool {
    self.set_remembered_idempotent()
  }

  #[inline]
  pub(crate) fn clear_remembered_idempotent(&self) {
    if self.is_forwarded() {
      return;
    }
    self.meta.fetch_and(!META_REMEMBERED, Ordering::Relaxed);
  }

  pub(crate) fn set_pinned(&mut self, pinned: bool) {
    debug_assert!(!self.is_forwarded(), "pinned objects must not be forwarded");
    if self.is_forwarded() {
      return;
    }
    if pinned {
      self.meta.fetch_or(META_PINNED, Ordering::Release);
    } else {
      self.meta.fetch_and(!META_PINNED, Ordering::Release);
    }
  }

  #[inline]
  pub(crate) fn mark_epoch(&self) -> u8 {
    ((self.meta.load(Ordering::Acquire) & META_MARK_MASK) >> META_MARK_SHIFT) as u8
  }

  #[inline]
  pub(crate) fn is_marked(&self, current_epoch: u8) -> bool {
    debug_assert!(current_epoch <= 1);
    self.mark_epoch() == current_epoch
  }

  #[inline]
  pub(crate) fn set_mark_epoch(&self, epoch: u8) {
    debug_assert!(epoch <= 1);
    if self.is_forwarded() {
      // Forwarded objects store relocation pointers in `meta` (used during minor
      // GC and optional major compaction). Mark bits are not meaningful on
      // forwarded from-space objects.
      return;
    }
    let mut meta = self.meta.load(Ordering::Relaxed);
    meta = (meta & !META_MARK_MASK) | ((epoch as usize) << META_MARK_SHIFT);
    self.meta.store(meta, Ordering::Release);
  }

  /// Returns a pointer to the per-object card table bitset, or null if the
  /// object has no card table.
  ///
  /// The pointer is stored in the high bits of `meta` (with the low flag bits
  /// reserved for GC metadata). As a result, card table allocations must be
  /// aligned such that the low [`META_FLAGS_MASK`] bits are zero.
  #[inline]
  #[doc(hidden)]
  pub fn card_table_ptr(&self) -> *mut AtomicU64 {
    if self.is_forwarded() {
      return core::ptr::null_mut();
    }
    let meta = self.meta.load(Ordering::Acquire);
    (meta & !META_FLAGS_MASK) as *mut AtomicU64
  }

  /// Install (or clear) the per-object card table pointer.
  ///
  /// # Safety
  /// The caller must ensure `ptr` points to a valid allocation containing at
  /// least `ceil(obj_size / CARD_SIZE).div_ceil(64)` [`AtomicU64`] words.
  #[inline]
  #[doc(hidden)]
  pub unsafe fn set_card_table_ptr(&mut self, ptr: *mut AtomicU64) {
    debug_assert!(
      (ptr as usize & META_FLAGS_MASK) == 0,
      "card table pointer must be aligned so low meta flag bits are free"
    );
    debug_assert!(
      !self.is_forwarded(),
      "forwarded headers must not carry card table pointers"
    );
    let flags = self.meta.load(Ordering::Relaxed) & META_FLAGS_MASK;
    self.meta.store((ptr as usize) | flags, Ordering::Release);
  }
}

/// Shape/type metadata required for precise tracing.
///
/// The offsets in `ptr_offsets` are byte offsets from the start of the object
/// (i.e. the address of [`ObjHeader`]) to each `*mut u8` pointer slot inside the
/// object.
///
/// IMPORTANT: `ptr_offsets` must list only **GC-managed** pointer fields (object references that
/// must be traced/updated by the collector). Pointers to non-GC memory (e.g. `ArrayBuffer` backing
/// stores allocated with malloc/mmap, kernel iovec buffers, etc.) must never appear here.
#[repr(C)]
pub struct TypeDescriptor {
  /// Total object size in bytes (including the [`ObjHeader`]).
  pub size: usize,
  /// Required alignment (in bytes) of the object base pointer.
  pub align: usize,
  ptr_offsets: *const u32,
  ptr_offsets_len: u32,
}

// `TypeDescriptor` is immutable runtime metadata. As long as the descriptor is
// constructed from stable, read-only data (the intended use-case), it is safe
// to share between threads.
unsafe impl Send for TypeDescriptor {}
unsafe impl Sync for TypeDescriptor {}

impl TypeDescriptor {
  pub const fn new(size: usize, ptr_offsets: &'static [u32]) -> Self {
    Self {
      size,
      align: OBJ_ALIGN,
      ptr_offsets: ptr_offsets.as_ptr(),
      ptr_offsets_len: ptr_offsets.len() as u32,
    }
  }

  pub const fn new_aligned(size: usize, align: usize, ptr_offsets: &'static [u32]) -> Self {
    Self {
      size,
      align: if align > OBJ_ALIGN { align } else { OBJ_ALIGN },
      ptr_offsets: ptr_offsets.as_ptr(),
      ptr_offsets_len: ptr_offsets.len() as u32,
    }
  }

  /// Construct a [`TypeDescriptor`] from raw pointer-offset metadata.
  ///
  /// # Safety
  /// - If `ptr_offsets_len != 0`, `ptr_offsets` must be a valid pointer to an array of
  ///   `ptr_offsets_len` `u32` elements.
  /// - The pointed-to array must remain valid and immutable for as long as this descriptor is used
  ///   (typically for the duration of the process).
  pub unsafe fn from_raw_parts(
    size: usize,
    align: usize,
    ptr_offsets: *const u32,
    ptr_offsets_len: u32,
  ) -> Self {
    Self {
      size,
      align: if align > OBJ_ALIGN { align } else { OBJ_ALIGN },
      ptr_offsets,
      ptr_offsets_len,
    }
  }

  #[inline]
  pub fn ptr_offsets(&self) -> &[u32] {
    if self.ptr_offsets_len == 0 {
      return &[];
    }
    debug_assert!(!self.ptr_offsets.is_null());
    // SAFETY: `ptr_offsets_len != 0` implies `ptr_offsets` is a valid pointer to an immutable
    // `u32` array per `TypeDescriptor::from_raw_parts`' safety contract.
    unsafe { slice::from_raw_parts(self.ptr_offsets, self.ptr_offsets_len as usize) }
  }
}

/// Common visitor interface used by both evacuation (minor GC) and marking
/// (major GC).
pub trait Tracer {
  /// Visit a slot that contains a GC reference.
  ///
  /// Implementations may update the slot in place (e.g. nursery evacuation).
  fn visit_slot(&mut self, slot: *mut *mut u8);

  /// Visit an object by scanning its pointer fields.
  fn visit_obj(&mut self, obj: *mut u8) {
    unsafe {
      for_each_ptr_slot(obj, |slot| self.visit_slot(slot));
    }
  }
}

/// Iterate over all pointer slots in `obj` as described by its
/// [`TypeDescriptor`].
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
/// - The object must be fully initialized, at least for all pointer slots.
pub(crate) unsafe fn for_each_ptr_slot(mut obj: *mut u8, mut f: impl FnMut(*mut *mut u8)) {
  debug_assert!(!obj.is_null());

  // Handle forwarding transparently: tracing should always operate on the
  // actual object body.
  let header = &*(obj as *const ObjHeader);
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = &*(obj as *const ObjHeader);
  let desc = header.type_desc();

  for &offset in desc.ptr_offsets() {
    let offset = offset as usize;
    debug_assert!(offset % mem::align_of::<*mut u8>() == 0);
    debug_assert!(offset + mem::size_of::<*mut u8>() <= desc.size);
    let slot = obj.add(offset) as *mut *mut u8;
    f(slot);
  }

  // Arrays have a dynamic pointer tail; special-case their element slots based on the header.
  if header.type_desc == &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
    array::for_each_ptr_elem_slot(obj, |slot| f(slot));
  }
}

/// Return the total size of the object at `obj` in bytes.
///
/// This is normally `obj.header.type_desc.size`, but some object kinds (notably arrays) have a
/// dynamic size derived from header fields.
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
pub(crate) unsafe fn obj_size(mut obj: *mut u8) -> usize {
  debug_assert!(!obj.is_null());

  // Follow forwarding pointers (nursery evacuation).
  let header = unsafe { &*(obj as *const ObjHeader) };
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = unsafe { &*(obj as *const ObjHeader) };
  if header.type_desc == &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
    return array::array_total_size_from_obj(obj);
  }
  unsafe { header.type_desc() }.size
}

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
mod verify;

/// Query the type-descriptor registry used by debug/verification tooling.
///
/// When the `conservative_roots` feature is enabled, conservative stack scanning
/// uses this to filter candidate pointers down to likely object headers.
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
pub(crate) fn is_known_type_descriptor(desc: *const TypeDescriptor) -> bool {
  verify::is_known_type_descriptor(desc)
}

/// Test/debug hook: execute `f` while holding the global known-type-descriptor registry lock.
///
/// This exists to deterministically force contention on the descriptor registry lock for
/// stop-the-world safepoint tests.
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
#[doc(hidden)]
pub fn debug_with_known_type_descriptors_lock<R>(f: impl FnOnce() -> R) -> R {
  verify::debug_with_known_type_descriptors_lock(f)
}

/// Test/debug hook: clear the "was contended" flag for the known-type-descriptor registry lock.
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
#[doc(hidden)]
pub fn debug_reset_known_type_descriptors_contention() {
  verify::debug_reset_known_type_descriptors_contention();
}

/// Test/debug hook: whether the known-type-descriptor registry lock has been observed contended.
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
#[doc(hidden)]
pub fn debug_known_type_descriptors_was_contended() -> bool {
  verify::debug_known_type_descriptors_was_contended()
}

#[cfg(not(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots")))]
pub(crate) fn is_known_type_descriptor(_desc: *const TypeDescriptor) -> bool {
  false
}

#[cfg(test)]
mod tests;
