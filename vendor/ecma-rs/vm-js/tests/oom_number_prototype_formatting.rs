//! Regression tests for OOM-safe Number formatting builtins.
//!
//! These tests install a custom `#[global_allocator]` so we can force *specific* allocations to
//! fail and assert that vm-js returns `VmError::OutOfMemory` instead of panicking/aborting.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);

static LOCK: Mutex<()> = Mutex::new(());

fn lock_allocator() -> MutexGuard<'static, ()> {
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_ALIGN.store(align, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && layout.size() == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && layout.size() == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && new_size == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed) {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      return std::ptr::null_mut();
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

fn assert_call_oom(rt: &mut JsRuntime, callee: Value, fail_alloc_size: usize) -> Result<(), VmError> {
  // `String`/`Vec<u8>` use `Layout { align: 1 }` for their backing buffers.
  fail_next_allocation(fail_alloc_size, 1);

  let vm = &mut rt.vm;
  let mut scope = rt.heap.scope();
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    vm.call_without_host(&mut scope, callee, Value::Undefined, &[])
  }));
  // Always clear the fail state so allocations during unwinding/test teardown don't get poisoned.
  FAIL_SIZE.store(0, Ordering::Relaxed);
  assert!(result.is_ok(), "execution must not panic");

  match result.unwrap() {
    Err(VmError::OutOfMemory) => Ok(()),
    Err(e) => Err(e),
    Ok(v) => panic!("expected VmError::OutOfMemory, got {v:?}"),
  }
}

#[test]
fn number_formatting_builtins_do_not_panic_on_allocator_oom() -> Result<(), VmError> {
  // Guard the global fail-state so we don't accidentally interfere with other threads in this
  // integration test binary.
  let _guard = lock_allocator();

  let mut rt = new_runtime();

  // Define callables once while allocations are enabled so we can trigger allocator OOM during the
  // Number formatting builtins without re-parsing scripts.
  rt.exec_script("globalThis.__to_fixed = () => (-0).toFixed(100);")?;
  rt.exec_script("globalThis.__to_exp = () => (1.23).toExponential(100);")?;
  rt.exec_script("globalThis.__to_prec = () => (1.23).toPrecision(100);")?;

  let to_fixed = rt.exec_script("__to_fixed")?;
  let to_exp = rt.exec_script("__to_exp")?;
  let to_prec = rt.exec_script("__to_prec")?;

  // These sizes correspond to the `String::try_reserve_exact` bounds in the Number formatting
  // builtins for the specific inputs used above.
  //
  // - (-0).toFixed(100): "0." + 100 digits = 102
  // - (1.23).toExponential(100): "d." + 100 digits + "e±d" = 105
  // - (1.23).toPrecision(100): "d." + 99 digits = 101
  assert_call_oom(&mut rt, to_fixed, 102)?;
  assert_call_oom(&mut rt, to_exp, 105)?;
  assert_call_oom(&mut rt, to_prec, 101)?;

  Ok(())
}
