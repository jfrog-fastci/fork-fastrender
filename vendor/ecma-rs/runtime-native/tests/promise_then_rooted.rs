use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<GcBox<u8>>() as u32,
      align: 16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

static FIRED: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_DATA: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_settle(data: *mut u8) {
  FIRED.fetch_add(1, Ordering::SeqCst);
  OBSERVED_DATA.store(data as usize, Ordering::SeqCst);
}

#[test]
fn promise_then_rooted_legacy_roots_data_until_invoked() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  // Allocate a pinned GC object so we can safely pass its base pointer into the rooted-then ABI.
  let shape = RtShapeId(1);
  let data = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  let promise = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_then_rooted_legacy(promise, on_settle, data);

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted promise then should allocate exactly one persistent handle for `data` while pending"
  );

  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    data as usize,
    "callback should receive the rooted GC base pointer"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted promise then should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

