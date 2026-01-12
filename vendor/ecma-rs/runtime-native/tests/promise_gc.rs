use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use runtime_native::abi::{LegacyPromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::gc::{ObjHeader, RememberedSet, RootSet, TypeDescriptor};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;

#[repr(C)]
struct BoxedU64 {
  header: ObjHeader,
  value: u64,
}

static NO_PTR_OFFSETS: [u32; 0] = [];
static BOXED_U64_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<BoxedU64>(), &NO_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

struct CapturedRoots {
  slots: Vec<*mut *mut u8>,
}

impl RootSet for CapturedRoots {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for &slot in &self.slots {
      f(slot);
    }
  }
}

fn collect_minor_from_world_roots(heap: &mut GcHeap) {
  runtime_native::threading::safepoint::with_world_stopped(|stop_epoch| {
    let mut slots: Vec<*mut *mut u8> = Vec::new();
    runtime_native::threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |slot| {
      let obj = unsafe { *slot };
      if obj.is_null() {
        return;
      }
      if heap.is_in_nursery(obj) {
        slots.push(slot);
      }
    })
    .expect("failed to enumerate root slots");

    let mut roots = CapturedRoots { slots };
    let mut remembered = NullRememberedSet::default();
    heap
      .collect_minor(&mut roots, &mut remembered)
      .expect("minor gc failed");
  });
}

#[repr(C)]
struct AwaitValueCoro {
  header: RtCoroutineHeader,
  awaited: LegacyPromiseRef,
  observed: *mut u8,
}

extern "C" fn await_value_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitValueCoro;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::RT_CORO_PENDING
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        (*coro).observed = (*coro).header.await_value.cast();
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::RT_CORO_DONE
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn legacy_promise_value_is_rooted_and_relocated_by_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let orig = heap.alloc_young(&BOXED_U64_DESC);
  let expected = 0xDEAD_BEEF_DEAD_BEEF;
  unsafe {
    (*(orig as *mut BoxedU64)).value = expected;
  }

  let awaited = runtime_native::rt_promise_new_legacy();

  let mut coro = Box::new(AwaitValueCoro {
    header: RtCoroutineHeader {
      resume: await_value_resume,
      promise: LegacyPromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited,
    observed: core::ptr::null_mut(),
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_resolve_legacy(awaited, orig.cast::<core::ffi::c_void>() as ValueRef);

  // Trigger a minor GC while the awaited promise is settled but its waiter reaction hasn't run yet.
  collect_minor_from_world_roots(&mut heap);

  while runtime_native::rt_async_poll_legacy() {}

  assert!(!coro.observed.is_null());
  assert_ne!(coro.observed, orig);
  assert!(!heap.is_in_nursery(coro.observed));
  assert!(heap.is_in_immix(coro.observed));
  let got = unsafe { (*(coro.observed as *const BoxedU64)).value };
  assert_eq!(got, expected);
}

static CONT_CALLED: AtomicBool = AtomicBool::new(false);
static CONT_SEEN_PTR: AtomicUsize = AtomicUsize::new(0);
static CONT_SEEN_VALUE: AtomicU64 = AtomicU64::new(0);

extern "C" fn continuation_observe_data(data: *mut u8) {
  CONT_CALLED.store(true, Ordering::SeqCst);
  CONT_SEEN_PTR.store(data as usize, Ordering::SeqCst);
  let obj = unsafe { &*(data as *const BoxedU64) };
  CONT_SEEN_VALUE.store(obj.value, Ordering::SeqCst);
}

#[test]
fn legacy_promise_then_rooted_keeps_gc_data_alive_across_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let orig = heap.alloc_young(&BOXED_U64_DESC);
  let expected = 0xCAFE_F00D_CAFE_F00D;
  unsafe {
    (*(orig as *mut BoxedU64)).value = expected;
  }

  CONT_CALLED.store(false, Ordering::SeqCst);
  CONT_SEEN_PTR.store(0, Ordering::SeqCst);
  CONT_SEEN_VALUE.store(0, Ordering::SeqCst);

  let p = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_then_rooted_legacy(p, continuation_observe_data, orig);

  // Evacuate the continuation data while the promise is still pending.
  collect_minor_from_world_roots(&mut heap);

  runtime_native::rt_promise_resolve_legacy(p, core::ptr::null_mut());
  while runtime_native::rt_async_poll_legacy() {}

  assert!(CONT_CALLED.load(Ordering::SeqCst));
  let seen = CONT_SEEN_PTR.load(Ordering::SeqCst) as *mut u8;
  assert_ne!(seen, orig);
  assert!(!heap.is_in_nursery(seen));
  assert!(heap.is_in_immix(seen));
  assert_eq!(CONT_SEEN_VALUE.load(Ordering::SeqCst), expected);
}
