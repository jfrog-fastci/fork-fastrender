//! GC root helpers for runtime-native code.
//!
//! LLVM statepoint stackmaps only cover *mutator stack/register* roots. The
//! runtime also needs to track:
//! - global/static roots (intern tables, singletons, ...)
//! - long-lived "handles" created by host code / FFI
//! - temporary roots created by runtime-native Rust code (without stackmaps)

/// Raw pointer to a GC-managed object (object base pointer).
///
/// A `GcPtr` points to the start of an object's [`crate::gc::ObjHeader`] prefix, not to the payload
/// after the header.
pub type GcPtr = runtime_native_abi::GcPtr;

/// A GC handle (pointer-to-slot).
///
/// This is the ABI-safe way to pass GC-managed pointers across any runtime entrypoint that may
/// allocate / trigger GC: the runtime can reload the pointer from the slot after a safepoint.
///
/// Note: this is *not* the same as the persistent [`crate::gc::HandleId`] (an integer handle stored
/// in [`RootRegistry`]). A `GcHandle` is just an addressable root slot (`*mut *mut u8`).
pub type GcHandle = runtime_native_abi::GcHandle;

/// Convert a mutable slot reference into a [`GcHandle`].
#[inline]
pub fn handle_from_slot(slot: &mut GcPtr) -> GcHandle {
  slot as *mut GcPtr
}

/// Load the current pointer from a [`GcHandle`].
///
/// # Safety
/// `h` must be valid for reads of a `GcPtr`.
#[inline]
pub unsafe fn load_handle(h: GcHandle) -> GcPtr {
  debug_assert!(!h.is_null(), "GcHandle must not be null");
  h.read()
}

/// Store a pointer into a [`GcHandle`]'s slot.
///
/// # Safety
/// `h` must be valid for writes of a `GcPtr`.
#[inline]
pub unsafe fn store_handle(h: GcHandle, ptr: GcPtr) {
  debug_assert!(!h.is_null(), "GcHandle must not be null");
  h.write(ptr);
}

pub mod registry;
pub mod persistent_handle_table;

pub use registry::{global_root_registry, RootRegistry, RootScope};
pub use persistent_handle_table::{global_persistent_handle_table, PersistentHandleTable};

/// Process-wide global/static GC roots.
///
/// This is a thin alias for [`RootRegistry`]. See [`register_global_root_slot`].
pub type GlobalRoots = RootRegistry;

pub mod conservative;
pub use conservative::conservative_scan_words;

// -----------------------------------------------------------------------------
// Global/static roots (always-scanned)
// -----------------------------------------------------------------------------

// Ensure it is sound to treat `usize` slots as `GcPtr` slots.
const _: [(); core::mem::size_of::<usize>()] = [(); core::mem::size_of::<GcPtr>()];
const _: [(); core::mem::align_of::<usize>()] = [(); core::mem::align_of::<GcPtr>()];

/// Register a *global* GC root slot.
///
/// This is intended for GC-managed pointers stored outside LLVM stackmaps:
/// - Rust `static`/singleton state
/// - long-lived runtime state structs
/// - TypeScript module/global variables stored in static memory
///
/// The registry stores **addresses of slots** (pointer-to-pointer). A relocating GC can update the
/// slot in place.
///
/// `slot` points to a `usize` containing a GC pointer value. The GC assumes the slot remains valid
/// and writable until it is unregistered.
pub fn register_global_root_slot(slot: *mut usize) {
  if slot.is_null() {
    std::process::abort();
  }
  if (slot as usize) % core::mem::align_of::<usize>() != 0 {
    std::process::abort();
  }
  // Reinterpret the `usize` slot as a `GcPtr` slot.
  let slot = slot as *mut GcPtr;
  let _handle = global_root_registry().register_root_slot(slot);
}

/// Unregister a *global* GC root slot previously registered via [`register_global_root_slot`].
pub fn unregister_global_root_slot(slot: *mut usize) {
  if slot.is_null() {
    std::process::abort();
  }
  if (slot as usize) % core::mem::align_of::<usize>() != 0 {
    std::process::abort();
  }
  let slot = slot as *mut GcPtr;
  global_root_registry().unregister_root_slot_ptr(slot);
}

/// Enumerate all GC root slots while the world is stopped.
///
/// This is a compatibility wrapper around
/// [`crate::threading::safepoint::for_each_root_slot_world_stopped`] that exposes root slots as
/// `*mut usize` so the visitor can update them in-place.
pub fn enumerate_root_slots_world_stopped(
  stop_epoch: u64,
  visit_root_slot: &mut dyn FnMut(*mut usize),
) -> Result<(), crate::WalkError> {
  crate::threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |slot| {
    visit_root_slot(slot as *mut usize);
  })
}

/// Stop the world, enumerate all GC root slots, then resume.
///
/// The GC typically calls [`enumerate_root_slots_world_stopped`] once it already holds a
/// stop-the-world pause; this helper is primarily intended for tests/debug tooling.
pub fn enumerate_root_slots(mut visit_root_slot: impl FnMut(*mut usize)) -> Result<(), crate::WalkError> {
  crate::threading::safepoint::with_world_stopped(|stop_epoch| {
    enumerate_root_slots_world_stopped(stop_epoch, &mut visit_root_slot)
  })
}

// -----------------------------------------------------------------------------
// Per-thread shadow stack roots (for Rust runtime code)
// -----------------------------------------------------------------------------

/// A temporary GC root handle for runtime-native Rust code.
///
/// Unlike TypeScript/LLVM-generated code, Rust code does not have stackmaps/statepoints on stable
/// Rust. Any GC-managed pointer held across a potential safepoint/GC must be explicitly rooted.
///
/// `Root<T>` stores the pointer in an internal, addressable slot and registers that slot in the
/// current thread's shadow stack (the per-thread handle stack in the thread registry). A relocating
/// GC updates the slot in-place.
///
/// # Panics
/// Panics if the current thread is not registered with `rt_thread_init` / `threading::register_current_thread`.
#[must_use]
pub struct Root<T> {
  slot: GcHandle,
  _marker: std::marker::PhantomData<T>,
  // Not Send/Sync: roots are tied to the current thread's shadow stack.
  _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl<T> Root<T> {
  pub fn new(ptr: *mut T) -> Self {
    let slot_box = Box::new(ptr as GcPtr);
    let slot: GcHandle = Box::into_raw(slot_box);

    let thread = crate::threading::registry::current_thread_state()
      .expect("Root<T> requires the current thread to be registered (call rt_thread_init)");
    thread.handle_stack_push(slot);

    Self {
      slot,
      _marker: std::marker::PhantomData,
      _not_send: std::marker::PhantomData,
    }
  }

  #[inline]
  pub fn get(&self) -> *mut T {
    // SAFETY: `self.slot` is a valid addressable slot for a `GcPtr`.
    unsafe { load_handle(self.slot) as *mut T }
  }

  #[inline]
  pub fn set(&mut self, new_ptr: *mut T) {
    // SAFETY: `self.slot` is a valid addressable slot for a `GcPtr`.
    unsafe {
      store_handle(self.slot, new_ptr as GcPtr);
    }
  }

  /// Returns the raw handle (address of the root slot).
  #[inline]
  pub fn handle(&self) -> GcHandle {
    self.slot
  }
}

impl<T> Drop for Root<T> {
  fn drop(&mut self) {
    if let Some(thread) = crate::threading::registry::current_thread_state() {
      thread.handle_stack_pop_checked(self.slot);
    }
    // SAFETY: `self.slot` was allocated by `Box::into_raw` in `new`.
    unsafe {
      drop(Box::from_raw(self.slot));
    }
  }
}

/// An address range (half-open `[start, end)`) representing (at least) the active GC heap.
///
/// Used by conservative root scanning to filter candidate pointers.
#[derive(Clone, Copy, Debug)]
pub struct HeapRange {
  start: *const u8,
  end: *const u8,
  is_object_start: Option<fn(*const u8) -> bool>,
}

impl HeapRange {
  /// Construct a heap range without an object-start check.
  ///
  /// The range is treated as half-open `[start, end)`.
  pub const fn new(start: *const u8, end: *const u8) -> Self {
    Self {
      start,
      end,
      is_object_start: None,
    }
  }

  /// Construct a heap range with an optional "object start" predicate.
  ///
  /// If provided, conservative scanning only reports candidates for which
  /// `is_object_start(candidate)` returns true.
  pub const fn with_object_start_check(
    start: *const u8,
    end: *const u8,
    is_object_start: fn(*const u8) -> bool,
  ) -> Self {
    Self {
      start,
      end,
      is_object_start: Some(is_object_start),
    }
  }

  #[inline]
  pub fn contains(self, ptr: *const u8) -> bool {
    let ptr = ptr as usize;
    let start = self.start as usize;
    let end = self.end as usize;
    start <= ptr && ptr < end
  }

  #[inline]
  pub(super) fn passes_object_start_check(self, candidate: *const u8) -> bool {
    match self.is_object_start {
      Some(is_object_start) => is_object_start(candidate),
      None => true,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn root_registry_can_be_cleared_for_tests() {
    // Smoke test for `clear_for_tests` wiring (used by `test_util::reset_runtime_state`).
    global_root_registry().clear_for_tests();
    let mut slot = 0usize as *mut u8;
    let handle = global_root_registry().register_root_slot(&mut slot as *mut *mut u8);
    global_root_registry().clear_for_tests();
    // Clearing should drop the entry; unregister should be a no-op now.
    global_root_registry().unregister(handle);
  }

  #[test]
  fn gc_handle_helpers_roundtrip() {
    let mut slot: GcPtr = 0x1234usize as *mut u8;
    let h = handle_from_slot(&mut slot);
    unsafe {
      assert_eq!(load_handle(h), 0x1234usize as *mut u8);
      store_handle(h, 0x5678usize as *mut u8);
    }
    assert_eq!(slot, 0x5678usize as *mut u8);
  }

  #[test]
  fn exported_may_gc_helpers_use_gc_handles_in_signatures() {
    // Compile-time ABI invariants: this assignment fails to compile if the exported function is
    // changed to accept a raw `*mut u8` instead of a handle.
    type Sig = unsafe extern "C" fn(GcHandle) -> GcPtr;
    let _f: Sig = crate::rt_gc_safepoint_relocate_h;
  }
}
