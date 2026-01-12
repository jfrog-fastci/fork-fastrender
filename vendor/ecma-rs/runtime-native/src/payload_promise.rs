use crate::abi::PromiseRef;
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING, PROMISE_FLAG_HAS_PAYLOAD};
use crate::async_runtime::PromiseLayout;
use crate::gc::GcHeap;
use crate::gc::TypeDescriptor;
use crate::trap;
use core::sync::atomic::{AtomicUsize, Ordering};
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
  let size = pp.payload_size;
  let align = pp.payload_align.max(1);

  if size == 0 || ptr.is_null() {
    return;
  }

  if !align.is_power_of_two() {
    // Corruption or ABI violation.
    std::process::abort();
  }

  let layout = Layout::from_size_align(size, align).unwrap_or_else(|_| std::process::abort());
  unsafe {
    std::alloc::dealloc(ptr, layout);
  }
  heap.sub_external_bytes(size);
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
  let payload_ptr = if layout.size == 0 {
    null_mut()
  } else {
    let buf_layout =
      Layout::from_size_align(layout.size, align).unwrap_or_else(|_| trap::rt_trap_invalid_arg("promise payload layout"));
    let ptr = unsafe { std::alloc::alloc_zeroed(buf_layout) };
    if ptr.is_null() {
      trap::rt_trap_oom(layout.size, "promise payload");
    }
    ptr
  };

  let promise = crate::rt_alloc::with_heap_lock_mutator(|heap| {
    // Allocate the promise object in the process-global heap.
    let obj = heap.alloc_old(&PAYLOAD_PROMISE_TYPE_DESC);
    let p = PromiseRef(obj.cast());

    unsafe {
      crate::native_async::promise_init(p);

      let pp = &mut *(obj as *mut PayloadPromise);
      pp.payload_ptr.store(payload_ptr as usize, Ordering::Relaxed);
      pp.payload_size = layout.size;
      pp.payload_align = align;

      // Publish payload fields before setting the `HAS_PAYLOAD` flag so an Acquire load of `flags`
      // also observes the payload pointer.
      let mut flags = PROMISE_FLAG_HAS_PAYLOAD;
      if external_pending {
        flags |= PROMISE_FLAG_EXTERNAL_PENDING;
      }
      pp.header.flags.store(flags, Ordering::Release);
    }

    heap.register_finalizer(obj, payload_promise_finalizer);
    if layout.size != 0 {
      heap.add_external_bytes(layout.size);
    }

    p
  });

  if external_pending && !promise.is_null() {
    crate::async_rt::external_pending_inc();
  }

  promise
}
