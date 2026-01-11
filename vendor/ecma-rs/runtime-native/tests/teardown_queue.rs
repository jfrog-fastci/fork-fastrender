use parking_lot::Mutex;
use runtime_native::async_rt::TeardownQueue;
use runtime_native::{HandleTable, OwnedGcHandle};
use std::ptr::NonNull;
use std::sync::Arc;

#[test]
fn teardown_discards_all_handles() {
  let table = Arc::new(Mutex::new(HandleTable::<usize>::new()));
  let mut queue = TeardownQueue::new();

  let mut ids = Vec::new();
  for i in 0..8usize {
    let ptr = NonNull::from(Box::leak(Box::new(i)));
    let handle = OwnedGcHandle::new(Arc::clone(&table), ptr);
    ids.push(handle.id());
    queue.push_back(handle);
  }

  queue.teardown();
  assert!(queue.is_empty());

  let table = table.lock();
  for id in ids {
    assert!(table.get(id).is_none());
  }
}

#[test]
fn drop_without_explicit_teardown_discards_all_handles() {
  let table = Arc::new(Mutex::new(HandleTable::<usize>::new()));

  let ids = {
    let mut queue = TeardownQueue::new();
    let mut ids = Vec::new();

    for i in 0..8usize {
      let ptr = NonNull::from(Box::leak(Box::new(i)));
      let handle = OwnedGcHandle::new(Arc::clone(&table), ptr);
      ids.push(handle.id());
      queue.push_back(handle);
    }

    ids
  };

  let table = table.lock();
  for id in ids {
    assert!(table.get(id).is_none());
  }
}
