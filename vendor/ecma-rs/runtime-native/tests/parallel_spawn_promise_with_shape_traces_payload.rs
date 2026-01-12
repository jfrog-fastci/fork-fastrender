use std::mem;
use std::sync::atomic::Ordering;
use std::sync::Once;
use std::time::{Duration, Instant};

use runtime_native::abi::{PromiseRef, RtShapeDescriptor, RtShapeId};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;

#[repr(C)]
struct PromiseWithPtrPayload {
  header: PromiseHeader,
  payload_ptr: *mut u8,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static PROMISE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(PromiseWithPtrPayload, payload_ptr) as u32];

const LEAF_SHAPE_ID: RtShapeId = RtShapeId(1);
const PROMISE_SHAPE_ID: RtShapeId = RtShapeId(2);

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 2] = [
      // Shape 1: leaf object (ObjHeader only).
      RtShapeDescriptor {
        size: mem::size_of::<ObjHeader>() as u32,
        align: mem::align_of::<ObjHeader>() as u16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: EMPTY_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
      // Shape 2: `PromiseHeader` prefix + one GC pointer in the payload.
      RtShapeDescriptor {
        size: mem::size_of::<PromiseWithPtrPayload>() as u32,
        align: mem::align_of::<PromiseWithPtrPayload>() as u16,
        flags: 0,
        ptr_offsets: PROMISE_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: PROMISE_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
    ];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

extern "C" fn write_gc_ptr_into_promise_payload(_data: *mut u8, promise: PromiseRef) {
  unsafe {
    // Root the promise pointer across `rt_alloc`: allocation may trigger a moving GC.
    let promise_root = runtime_native::roots::Root::<u8>::new(promise.0.cast());

    let obj = runtime_native::rt_alloc(mem::size_of::<ObjHeader>(), LEAF_SHAPE_ID);
    assert!(!obj.is_null());

    let promise = PromiseRef(promise_root.get().cast());

    let slot = runtime_native::rt_promise_payload_ptr(promise) as *mut *mut u8;
    assert!(!slot.is_null());
    *slot = obj;

    // Mirror compiler-inserted write barrier semantics: storing a GC pointer into a GC-managed
    // promise payload must record old→young pointers when the promise is promoted.
    runtime_native::rt_write_barrier(promise.0.cast::<u8>(), slot.cast::<u8>());

    runtime_native::rt_promise_fulfill(promise);
  }
}

struct WeakHandleGuard(u64);

impl WeakHandleGuard {
  fn new_from_slot(slot: runtime_native::roots::GcHandle) -> Self {
    let handle = unsafe { runtime_native::rt_weak_add_h(slot) };
    Self(handle)
  }

  fn get(&self) -> *mut u8 {
    runtime_native::rt_weak_get(self.0)
  }
}

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    runtime_native::rt_weak_remove(self.0);
  }
}

#[test]
fn parallel_spawn_promise_with_shape_traces_payload() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  let promise = runtime_native::rt_parallel_spawn_promise_with_shape(
    write_gc_ptr_into_promise_payload,
    core::ptr::null_mut(),
    mem::size_of::<PromiseWithPtrPayload>(),
    mem::align_of::<PromiseWithPtrPayload>(),
    PROMISE_SHAPE_ID,
  );
  assert!(!promise.is_null());

  // Keep the promise pointer live and relocatable across potential moving collections while the
  // worker is running.
  let promise_root = runtime_native::roots::Root::<PromiseHeader>::new(promise.0.cast());

  // Wait until the worker fulfills the promise.
  const TIMEOUT: Duration = Duration::from_secs(5);
  let start = Instant::now();
  loop {
    let p = promise_root.get();
    let state = unsafe { &(*p).state }.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED {
      break;
    }
    if start.elapsed() > TIMEOUT {
      panic!("timeout waiting for promise to fulfill");
    }
    std::thread::yield_now();
  }

  // Read the GC pointer stored by the worker.
  let promise_for_read = PromiseRef(promise_root.get().cast());
  let payload = runtime_native::rt_promise_payload_ptr(promise_for_read) as *mut *mut u8;
  assert!(!payload.is_null());
  let obj_before = unsafe { *payload };
  assert!(!obj_before.is_null());

  // Create a weak handle to the referent, then drop any other roots: the object must stay alive
  // solely because the promise payload is traced.
  let obj_root = runtime_native::roots::Root::<u8>::new(obj_before);
  let weak = WeakHandleGuard::new_from_slot(obj_root.handle());
  drop(obj_root);

  runtime_native::rt_gc_collect();

  let obj_after = weak.get();
  assert!(
    !obj_after.is_null(),
    "object referenced only by promise payload should remain alive after GC"
  );

  // The GC must also update the pointer slot inside the promise payload if the referent is moved.
  let promise_after_gc = PromiseRef(promise_root.get().cast());
  let payload_after_gc = runtime_native::rt_promise_payload_ptr(promise_after_gc) as *mut *mut u8;
  assert!(!payload_after_gc.is_null());
  let obj_in_payload = unsafe { *payload_after_gc };
  assert_eq!(
    obj_in_payload, obj_after,
    "promise payload pointer slot should be updated by GC relocation"
  );
}
