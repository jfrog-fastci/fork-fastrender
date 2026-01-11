//! GC root helpers for runtime-native code.
//!
//! LLVM statepoint stackmaps only cover *mutator stack/register* roots. The
//! runtime also needs to track:
//! - global/static roots (intern tables, singletons, ...)
//! - long-lived "handles" created by host code / FFI
//! - temporary roots created by runtime-native Rust code (without stackmaps)

pub mod registry;

pub use registry::{global_root_registry, RootRegistry, RootScope};

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
}

