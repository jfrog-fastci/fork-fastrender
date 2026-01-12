use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

extern "C" fn fulfill_and_count(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passes `Arc::into_raw(counter.clone()) as *mut u8`.
  let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise);
    if !payload.is_null() {
      payload.write_volatile(0xAA);
    }
    runtime_native::rt_promise_fulfill(promise);
  }
  counter.fetch_add(1, Ordering::Release);
  // `counter` dropped here.
}

#[test]
fn parallel_spawn_promise_payload_buffers_are_reclaimed_after_gc() {
  let _rt = TestRuntimeGuard::new();

  // Ensure worker threads are initialized so one-time setup doesn't perturb the baseline accounting.
  extern "C" fn noop(_data: *mut u8) {}
  let warmup = runtime_native::rt_parallel_spawn(noop, core::ptr::null_mut());
  runtime_native::rt_parallel_join(&warmup as *const runtime_native::abi::TaskId, 1);

  // Clear any transient allocations and force finalizers for prior tests.
  runtime_native::rt_gc_collect();

  let baseline_roots = runtime_native::roots::global_persistent_handle_table().live_count();
  let baseline_external_bytes = runtime_native::rt_debug_heap_external_bytes();

  const TASKS: usize = 128;
  const PAYLOAD_BYTES: usize = 4096;

  let counter = Arc::new(AtomicUsize::new(0));
  for _ in 0..TASKS {
    let _ = runtime_native::rt_parallel_spawn_promise(
      fulfill_and_count,
      Arc::into_raw(counter.clone()) as *mut u8,
      PromiseLayout {
        size: PAYLOAD_BYTES,
        align: 16,
      },
    );
  }

  let during_bytes = runtime_native::rt_debug_heap_external_bytes();
  assert!(
    during_bytes >= baseline_external_bytes + TASKS * PAYLOAD_BYTES,
    "expected payload promises to contribute to external byte accounting (baseline={baseline_external_bytes}, during={during_bytes})"
  );

  let deadline = Instant::now() + Duration::from_secs(10);
  while counter.load(Ordering::Acquire) < TASKS {
    assert!(
      Instant::now() < deadline,
      "timeout waiting for parallel payload promise tasks to complete"
    );
    std::thread::yield_now();
  }

  // Wait for promise roots to be released (they are freed after the worker callback returns).
  let deadline = Instant::now() + Duration::from_secs(10);
  while runtime_native::roots::global_persistent_handle_table().live_count() != baseline_roots {
    assert!(
      Instant::now() < deadline,
      "timeout waiting for payload promise persistent roots to be released"
    );
    std::thread::yield_now();
  }

  // After the promises become unreachable, a major GC should run their finalizers and reclaim their
  // external payload buffers.
  runtime_native::rt_gc_collect();

  let after_bytes = runtime_native::rt_debug_heap_external_bytes();
  assert!(
    after_bytes <= baseline_external_bytes,
    "expected payload promise buffers to be reclaimed after GC (baseline={baseline_external_bytes}, after={after_bytes})"
  );
}

