//! GC root helpers for runtime-native code.
//!
//! LLVM statepoint stackmaps only cover *mutator stack/register* roots. The
//! runtime also needs to track:
//! - global/static roots (intern tables, singletons, ...)
//! - long-lived "handles" created by host code / FFI
//! - temporary roots created by runtime-native Rust code (without stackmaps)

/// Raw pointer to a GC-managed object.
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

pub mod conservative;
pub use conservative::conservative_scan_words;

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
