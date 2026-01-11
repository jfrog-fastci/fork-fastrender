use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;
use runtime_native::TypeDescriptor;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const MAGIC: u64 = 0x0123_4567_89AB_CDEF;

const HEADER_SIZE: usize = std::mem::size_of::<ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;
const SEEN_OFFSET: usize = HEADER_SIZE + std::mem::size_of::<u64>();

static NO_PTR_OFFSETS: [u32; 0] = [];
static TEST_OBJ_DESC: TypeDescriptor = TypeDescriptor::new(
  HEADER_SIZE + std::mem::size_of::<u64>() + std::mem::size_of::<AtomicU64>(),
  &NO_PTR_OFFSETS,
);

static SELF_CLEAR_INTERVAL_ID: AtomicU64 = AtomicU64::new(0);

unsafe fn init_test_obj(heap: &mut GcHeap) -> *mut u8 {
  let obj = heap.alloc_young(&TEST_OBJ_DESC);
  (obj.add(MAGIC_OFFSET) as *mut u64).write(MAGIC);
  (obj.add(SEEN_OFFSET) as *mut AtomicU64).write(AtomicU64::new(0));
  obj
}

unsafe fn seen_magic_slot(obj: *mut u8) -> &'static AtomicU64 {
  &*(obj.add(SEEN_OFFSET) as *const AtomicU64)
}

extern "C" fn record_magic(data: *mut u8) {
  unsafe {
    let magic = (data.add(MAGIC_OFFSET) as *const u64).read();
    let seen = &*(data.add(SEEN_OFFSET) as *const AtomicU64);
    seen.store(magic, Ordering::Release);
  }
}

extern "C" fn record_magic_and_clear_self(data: *mut u8) {
  record_magic(data);
  let id = SELF_CLEAR_INTERVAL_ID.load(Ordering::Acquire);
  assert_ne!(id, 0, "interval id not set before callback");
  runtime_native::rt_clear_timer(id);
}

fn collect_major(heap: &mut GcHeap) {
  let mut roots = GlobalRootSet::new();
  let mut remembered = SimpleRememberedSet::new();
  let _ = heap.collect_major(&mut roots, &mut remembered);
}

struct WeakHandleGuard(u64);

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

#[test]
fn timeout_rooted_keeps_gc_object_alive_and_relocates_pointer() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let _timer = runtime_native::rt_set_timeout_rooted(record_magic, obj, 0);

  // Move/collect while the timer is still pending.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "rooted timeout did not run in time");
    std::thread::yield_now();
  }

  // After the timeout executes, its root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted timeout executed (root not released?)"
    );
    std::thread::yield_now();
  }
}

#[test]
fn interval_rooted_keeps_gc_object_alive_until_cleared() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = runtime_native::rt_set_interval_rooted(record_magic, obj, 0);

  // Move/collect while the interval is registered.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "rooted interval did not fire in time");
    std::thread::yield_now();
  }

  runtime_native::rt_clear_timer(id);

  // After the interval is cleared, its root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted interval was cleared (root not released?)"
    );
    std::thread::yield_now();
  }
}

#[test]
fn timeout_rooted_can_be_cleared_before_fire_and_releases_root() {
  static FIRED: AtomicU64 = AtomicU64::new(0);
  extern "C" fn mark_fired(_data: *mut u8) {
    FIRED.fetch_add(1, Ordering::AcqRel);
  }

  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  // Schedule an immediately-due timeout, then clear it before the event loop runs.
  let id = runtime_native::rt_set_timeout_rooted(mark_fired, obj, 0);

  // Move/collect while the timeout is still registered but before it fires.
  collect_major(&mut heap);
  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  runtime_native::rt_clear_timer(id);

  // Run the event loop; the callback must not execute.
  for _ in 0..10 {
    runtime_native::rt_async_poll_legacy();
    std::thread::yield_now();
  }
  assert_eq!(
    FIRED.load(Ordering::Acquire),
    0,
    "rooted timeout callback ran even though it was cleared"
  );

  // Root must be released after clear, so the object should become collectible.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted timeout was cleared (root not released?)"
    );
    std::thread::yield_now();
  }
}

#[test]
fn interval_rooted_can_clear_itself_and_releases_root() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = runtime_native::rt_set_interval_rooted(record_magic_and_clear_self, obj, 0);
  SELF_CLEAR_INTERVAL_ID.store(id, Ordering::Release);

  // Move/collect while the interval is registered but before it fires.
  collect_major(&mut heap);

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(
      Instant::now() < deadline,
      "self-clearing rooted interval did not fire in time"
    );
    std::thread::yield_now();
  }

  // Drain the event loop so any deferred interval teardown completes.
  while runtime_native::rt_async_poll_legacy() {
    std::thread::yield_now();
  }

  // After the interval clears itself, the root must be released and the object should become
  // collectible.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted interval cleared itself (root not released?)"
    );
    std::thread::yield_now();
  }
}
