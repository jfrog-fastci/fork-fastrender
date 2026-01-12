use crate::abi::PromiseRef;
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING, PROMISE_FLAG_HAS_PAYLOAD};
use crate::async_runtime::PromiseLayout;
use crate::gc::GcHeap;
use crate::gc::TypeDescriptor;
use crate::trap;
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(unix))]
use std::alloc::Layout;
use std::ptr::null_mut;

/// GC-managed promise object used by `rt_parallel_spawn_promise*`.
///
/// # Layout / ABI
/// `PromiseRef` is an ABI-level opaque pointer that must point at a [`PromiseHeader`] at offset 0.
/// `PayloadPromise` therefore embeds the header as its first field.
///
/// The promise's payload is stored out-of-line (allocated outside the GC heap) and is accessible via
/// `rt_promise_payload_ptr`.
#[repr(C)]
pub(crate) struct PayloadPromise {
  pub(crate) header: PromiseHeader,
  /// Pointer to the external payload buffer.
  ///
  /// This is *not* a GC-managed pointer and must not appear in the [`TypeDescriptor`] pointer
  /// offsets.
  ///
  /// NOTE: stored in an atomic to match the contract assumed by `async_rt::promise::classify_promise`
  /// for all `PROMISE_FLAG_HAS_PAYLOAD` promises.
  pub(crate) payload_ptr: AtomicUsize,
  /// Base pointer to the allocated payload buffer (for freeing).
  ///
  /// On Unix we allocate payload buffers with `mmap` to avoid relying on the Rust
  /// global allocator during stop-the-world GC finalizers; `payload_ptr` may be
  /// an aligned pointer into the mapping, while `payload_base_ptr` points at the
  /// start of the mapping passed to `munmap`.
  pub(crate) payload_base_ptr: usize,
  pub(crate) payload_size: usize,
  pub(crate) payload_align: usize,
}

static NO_PTR_OFFSETS: [u32; 0] = [];

pub(crate) static PAYLOAD_PROMISE_TYPE_DESC: TypeDescriptor = TypeDescriptor::new_aligned(
  core::mem::size_of::<PayloadPromise>(),
  core::mem::align_of::<PayloadPromise>(),
  &NO_PTR_OFFSETS,
);

unsafe fn payload_promise_finalizer(heap: &mut GcHeap, obj: *mut u8) {
  if obj.is_null() {
    return;
  }

  // SAFETY: `obj` is expected to be a live `PayloadPromise` object base pointer at the time the
  // finalizer runs.
  let pp = unsafe { &*(obj as *const PayloadPromise) };
  let ptr = pp.payload_ptr.load(Ordering::Acquire) as *mut u8;
  let base_ptr = pp.payload_base_ptr as *mut u8;
  let size = pp.payload_size;
  let _align = pp.payload_align.max(1);

  if size == 0 || ptr.is_null() {
    return;
  }

  // `payload_base_ptr` is allowed to be null only when `payload_ptr` is null.
  if base_ptr.is_null() {
    std::process::abort();
  }

  #[cfg(unix)]
  unsafe {
    // `munmap` requires the original mapping base pointer. For aligned payload pointers (align >
    // page size) we may have returned an interior pointer via `payload_ptr`.
    munmap_payload(base_ptr, size);
    heap.sub_external_bytes(size);
    return;
  }

  #[cfg(not(unix))]
  unsafe {
    if !_align.is_power_of_two() {
      // Corruption or ABI violation.
      std::process::abort();
    }
    let layout = Layout::from_size_align(size, _align).unwrap_or_else(|_| std::process::abort());
    std::alloc::dealloc(ptr, layout);
    heap.sub_external_bytes(size);
  }
}

#[cfg(unix)]
fn page_size() -> usize {
  // SAFETY: sysconf is thread-safe.
  let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if sz <= 0 { 4096 } else { sz as usize }
}

#[cfg(unix)]
fn round_up_to_page_size(bytes: usize) -> usize {
  let page = page_size();
  bytes
    .checked_add(page - 1)
    .map(|v| v & !(page - 1))
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("promise payload size overflow"))
}

#[cfg(unix)]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  addr
    .checked_add(align - 1)
    .map(|v| v & !(align - 1))
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("promise payload size overflow"))
}

#[cfg(unix)]
unsafe fn mmap_payload(size: usize, align: usize) -> (*mut u8, *mut u8, usize) {
  debug_assert!(size != 0);
  debug_assert!(align != 0 && align.is_power_of_two());

  let page = page_size();

  // `mmap` returns a page-aligned pointer. For alignments up to the page size, this is sufficient.
  // For larger alignments, over-allocate so we can pick an aligned interior pointer while still
  // tracking the original mapping base for `munmap`.
  let required = if align <= page {
    size
  } else {
    size
      .checked_add(align - 1)
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("promise payload size overflow"))
  };
  let map_len = round_up_to_page_size(required);

  let base = loop {
    let raw = libc::mmap(
      core::ptr::null_mut(),
      map_len,
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
      let _ = libc::munmap(raw, map_len);
      break raw;
    }
    break raw;
  };

  if base == libc::MAP_FAILED || base.is_null() {
    trap::rt_trap_oom(map_len, "promise payload");
  }

  let base_ptr = base as *mut u8;
  let user_ptr = if align <= page {
    base_ptr
  } else {
    align_up(base_ptr as usize, align) as *mut u8
  };

  // `map_len` was computed to ensure this holds.
  debug_assert!(
    (user_ptr as usize)
      .checked_add(size)
      .is_some_and(|end| end <= (base_ptr as usize + map_len)),
    "promise payload mapping too small"
  );

  (user_ptr, base_ptr, map_len)
}

#[cfg(unix)]
unsafe fn munmap_payload(ptr: *mut u8, len: usize) {
  if ptr.is_null() || len == 0 {
    return;
  }
  loop {
    let rc = libc::munmap(ptr.cast(), len);
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
      eprintln!("runtime-native: munmap(promise payload) failed: {err}");
    }
    std::process::abort();
  }
}

/// Allocate a new pending payload promise and its out-of-line payload buffer.
///
/// The returned promise is GC-managed (allocated in the process-global heap) and has a GC finalizer
/// registered to free the external payload buffer when the promise becomes unreachable.
pub(crate) fn alloc_payload_promise(layout: PromiseLayout, external_pending: bool) -> PromiseRef {
  let align = layout.align.max(1);
  if !align.is_power_of_two() {
    trap::rt_trap_invalid_arg("promise payload align must be a power of two");
  }

  // Allocate the payload buffer outside the GC heap.
  let (payload_ptr, payload_base_ptr, payload_alloc_len) = if layout.size == 0 {
    (null_mut(), null_mut(), 0usize)
  } else {
    #[cfg(unix)]
    unsafe {
      let (user, base, len) = mmap_payload(layout.size, align);
      (user, base, len)
    }
    #[cfg(not(unix))]
    {
      let buf_layout = Layout::from_size_align(layout.size, align)
        .unwrap_or_else(|_| trap::rt_trap_invalid_arg("promise payload layout"));
      let ptr = unsafe { std::alloc::alloc_zeroed(buf_layout) };
      if ptr.is_null() {
        trap::rt_trap_oom(layout.size, "promise payload");
      }
      (ptr, ptr, layout.size)
    }
  };

  // Allocate the promise object in the GC heap as a normal movable object (nursery preferred).
  //
  // Payload promises must be movable under minor GC evacuation and (optional future) major
  // compaction; callers root them via the persistent handle table while tasks are pending.
  let mut obj = crate::rt_alloc::alloc_typed(&PAYLOAD_PROMISE_TYPE_DESC);
  let promise = PromiseRef(obj.cast());

  unsafe {
    crate::native_async::promise_init(promise);

    let pp = &mut *(obj as *mut PayloadPromise);
    pp.payload_ptr.store(payload_ptr as usize, Ordering::Relaxed);
    pp.payload_base_ptr = payload_base_ptr as usize;
    pp.payload_size = payload_alloc_len;
    pp.payload_align = align;

    // Publish payload fields before setting the `HAS_PAYLOAD` flag so an Acquire load of `flags`
    // also observes the payload pointer.
    let mut flags = PROMISE_FLAG_HAS_PAYLOAD;
    if external_pending {
      flags |= PROMISE_FLAG_EXTERNAL_PENDING;
    }
    pp.header.flags.store(flags, Ordering::Release);
  }

  // Register a finalizer and account for the external payload buffer. Acquiring the heap lock is
  // GC-aware; if contended it may temporarily enter a GC-safe region while waiting. Root `obj` in an
  // addressable slot so a moving GC can update it before we register the finalizer.
  let slot = &mut obj as *mut *mut u8;
  let mut scope = crate::roots::RootScope::new();
  scope.push(slot);
  crate::rt_alloc::with_global_heap_lock_mutator(|heap| unsafe {
    let obj = slot.read();
    heap.register_finalizer(obj, payload_promise_finalizer);
    if payload_alloc_len != 0 {
      heap.add_external_bytes(payload_alloc_len);
    }
  });
  drop(scope);

  if external_pending && !promise.is_null() {
    crate::async_rt::external_pending_inc();
  }

  promise
}
