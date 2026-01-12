//! Regression tests for OOM-safe runtime error message formatting.
//!
//! These tests install a custom `#[global_allocator]` so we can force *specific* allocations to
//! fail and assert that vm-js returns `VmError::OutOfMemory` instead of aborting the process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAIL_SKIP_MATCHES: AtomicUsize = AtomicUsize::new(0);

static LOCK: Mutex<()> = Mutex::new(());

fn lock_allocator() -> MutexGuard<'static, ()> {
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SKIP_MATCHES.store(0, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && layout.size() == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
      } else {
        FAIL_SIZE.store(0, Ordering::Relaxed);
        return std::ptr::null_mut();
      }
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && layout.size() == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
      } else {
        FAIL_SIZE.store(0, Ordering::Relaxed);
        return std::ptr::null_mut();
      }
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && new_size == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
      } else {
        FAIL_SIZE.store(0, Ordering::Relaxed);
        return std::ptr::null_mut();
      }
    }
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: FailingAllocator = FailingAllocator;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn large_identifier_reference_error_message_allocation_is_fallible() {
  let _guard = lock_allocator();

  // Large, attacker-controlled identifier that will be embedded into the ReferenceError message.
  let ident = "a".repeat(3000);
  let source = format!("{ident};");

  // The error message is formatted as: `"{ident} is not defined"`.
  let expected_len = ident.len() + " is not defined".len();
  let mut rt = new_runtime();
  fail_next_allocation(expected_len, 1);
  let err = rt.exec_script(&source).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}

#[test]
fn large_property_name_type_error_message_allocation_is_fallible() {
  let _guard = lock_allocator();

  // Large, attacker-controlled global property name.
  let ident = "b".repeat(3000);
  let setup = format!(
    r#"Object.defineProperty(globalThis, "{ident}", {{ value: 1, writable: false }});"#
  );
  let assign = format!(r#""use strict"; {ident} = 2;"#);

  // The error message is formatted as:
  // `"Cannot assign to read only property '{ident}'"`.
  let expected_len = "Cannot assign to read only property '".len() + ident.len() + "'".len();
  let mut rt = new_runtime();
  rt.exec_script(&setup).unwrap();
  fail_next_allocation(expected_len, 1);
  let err = rt.exec_script(&assign).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}
