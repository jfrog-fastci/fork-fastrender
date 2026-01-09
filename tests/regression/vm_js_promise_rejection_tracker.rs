use vm_js::{
  Heap, HeapLimits, PromiseHandle, PromiseRejectionHandleAction, PromiseRejectionTracker,
};

#[test]
fn promise_rejection_tracker_api_smoke() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let promise: PromiseHandle;
  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object().unwrap();
    promise = PromiseHandle::from(obj);
  }

  let mut tracker = PromiseRejectionTracker::new();
  tracker.on_reject(&mut heap, promise);

  let batch = tracker.drain_about_to_be_notified(&mut heap);
  assert_eq!(batch.promises(), &[promise]);
  batch.teardown(&mut heap);

  tracker.after_unhandledrejection_dispatch(promise, false);
  assert_eq!(
    tracker.on_handle(&mut heap, promise),
    PromiseRejectionHandleAction::QueueRejectionHandled { promise }
  );
}
