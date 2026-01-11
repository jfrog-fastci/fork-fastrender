use vm_js::{Heap, HeapLimits, Value, VmError};

#[test]
fn array_buffer_external_bytes_freed_on_gc() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024 * 1024, 1024 * 1024 * 1024));

  // Allocate a bunch of unrooted buffers. They should contribute to the heap's external byte
  // counter until a GC cycle runs.
  {
    let mut scope = heap.scope();
    let count = 16usize;
    let size = 1024 * 1024; // 1 MiB
    for _ in 0..count {
      scope.alloc_array_buffer(size)?;
    }
    assert_eq!(scope.heap().external_bytes(), count * size);
  }

  heap.collect_garbage();
  assert_eq!(heap.external_bytes(), 0);
  Ok(())
}

#[test]
fn array_buffer_finalizer_runs_once() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024 * 1024, 1024 * 1024 * 1024));

  let size = 256 * 1024;
  {
    let mut scope = heap.scope();
    let buf = scope.alloc_array_buffer(size)?;
    scope.push_root(Value::Object(buf))?;

    // Force GC while the ArrayBuffer is still reachable.
    scope.heap_mut().collect_garbage();
    assert_eq!(scope.heap().external_bytes(), size);
  }

  // Once the scope is dropped the ArrayBuffer becomes unreachable; the next GC should run its
  // finalizer and release the backing store.
  heap.collect_garbage();
  assert_eq!(heap.external_bytes(), 0);

  // A subsequent GC should not run the finalizer again.
  heap.collect_garbage();
  assert_eq!(heap.external_bytes(), 0);
  Ok(())
}

#[test]
fn heap_limits_account_for_external_bytes() -> Result<(), VmError> {
  // Keep the limit comfortably above metadata overhead but below 2x the buffer size so the second
  // allocation must fail due to external memory usage.
  let max_bytes = 1024 * 1024; // 1 MiB
  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let mut scope = heap.scope();

  let size = 600 * 1024;
  let buf = scope.alloc_array_buffer(size)?;
  scope.push_root(Value::Object(buf))?;
  assert_eq!(scope.heap().external_bytes(), size);

  match scope.alloc_array_buffer(size) {
    Err(VmError::OutOfMemory) => {}
    Ok(_) => panic!("expected second large ArrayBuffer allocation to hit VmError::OutOfMemory"),
    Err(e) => return Err(e),
  }

  // The failed allocation should not leak external bytes.
  assert_eq!(scope.heap().external_bytes(), size);
  Ok(())
}

