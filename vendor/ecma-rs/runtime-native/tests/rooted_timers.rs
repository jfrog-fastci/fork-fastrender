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
