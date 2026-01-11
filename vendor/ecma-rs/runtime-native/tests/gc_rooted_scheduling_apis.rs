#![cfg(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::gc::ObjHeader;
use runtime_native::gc::TypeDescriptor;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::GcHeap;
use std::sync::atomic::{AtomicUsize, Ordering};

static OBSERVED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_ptr(data: *mut u8) {
  OBSERVED.store(data as usize, Ordering::SeqCst);
}

#[repr(C)]
struct Leaf {
  _header: ObjHeader,
}

static LEAF_DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<Leaf>(), &[]);

fn simulate_relocation(old_ptr: *mut u8, new_ptr: *mut u8) {
  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == old_ptr {
        *slot = new_ptr;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });

  assert_eq!(updated, 1, "expected exactly one persistent-handle slot update");
}

#[test]
fn queue_microtask_handle_reloads_userdata_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  runtime_native::rt_queue_microtask_handle(record_ptr, h);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the microtask runs"
  );

  threading::unregister_current_thread();
}

#[test]
fn set_timeout_handle_reloads_userdata_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  let _timer = runtime_native::rt_set_timeout_handle(record_ptr, h, 0);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the timeout fires"
  );

  threading::unregister_current_thread();
}

