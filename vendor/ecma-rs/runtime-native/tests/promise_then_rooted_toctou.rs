use runtime_native::abi::{PromiseRef, RtShapeDescriptor, RtShapeId};
use runtime_native::rt_alloc;
use runtime_native::rt_gc_collect;
use runtime_native::rt_gc_get_young_range;
use runtime_native::rt_promise_fulfill;
use runtime_native::rt_promise_init;
use runtime_native::rt_promise_then_rooted_legacy;
use runtime_native::rt_root_pop;
use runtime_native::rt_root_push;
use runtime_native::rt_weak_add;
use runtime_native::rt_weak_get;
use runtime_native::rt_weak_remove;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

const MAGIC: u64 = 0x55AA_F00D_1234_5678;
const HEADER_SIZE: usize = core::mem::size_of::<runtime_native::gc::ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;

static OBSERVED_PTR: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_MAGIC: AtomicU64 = AtomicU64::new(0);

extern "C" fn record_ptr_and_magic(data: *mut u8) {
  // Read a marker from the GC-managed `data` pointer and record both it and the pointer value.
  let magic = unsafe { (data.add(MAGIC_OFFSET) as *const u64).read() };
  OBSERVED_MAGIC.store(magic, Ordering::Relaxed);
  OBSERVED_PTR.store(data as usize, Ordering::Release);
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  // Small object that should allocate in the nursery.
  size: 64,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

struct RootSlotGuard {
  slot: *mut *mut u8,
}
impl Drop for RootSlotGuard {
  fn drop(&mut self) {
    unsafe {
      rt_root_pop(self.slot);
    }
  }
}

struct WeakHandleGuard(u64);
impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

#[test]
fn promise_then_rooted_is_safe_when_root_registration_blocks_under_moving_gc() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the current thread claims the event-loop identity so other threads register as `External`
  // rather than becoming the event loop.
  let _ = runtime_native::rt_async_poll();

  ensure_shape_table();

  OBSERVED_PTR.store(0, Ordering::SeqCst);
  OBSERVED_MAGIC.store(0, Ordering::SeqCst);

  // Allocate a GC-managed promise in the nursery.
  let mut promise_obj = rt_alloc(SHAPES[0].size as usize, RtShapeId(1));
  assert!(!promise_obj.is_null());
  let promise_pre_gc = promise_obj as usize;

  // Allocate a GC-managed data object and mark it.
  let mut data_obj = rt_alloc(SHAPES[0].size as usize, RtShapeId(1));
  assert!(!data_obj.is_null());
  let data_pre_gc = data_obj as usize;
  unsafe {
    (data_obj.add(MAGIC_OFFSET) as *mut u64).write(MAGIC);
  }

  // Root both objects for the duration of the test so they survive the moving GC and so we can
  // observe relocation through updated slots.
  unsafe {
    rt_root_push(&mut promise_obj as *mut *mut u8);
    rt_root_push(&mut data_obj as *mut *mut u8);
  }
  let _promise_guard = RootSlotGuard {
    slot: &mut promise_obj as *mut *mut u8,
  };
  let _data_guard = RootSlotGuard {
    slot: &mut data_obj as *mut *mut u8,
  };

  // Initialize the promise header.
  let promise_handle = PromiseRef(promise_obj.cast());
  unsafe {
    rt_promise_init(promise_handle);
  }

  // Track relocation via weak handles so we can observe updated pointers without relying on the GC
  // mutating Rust locals (which would be a data race in Rust's memory model).
  let weak_promise = rt_weak_add(promise_obj);
  let _weak_promise_guard = WeakHandleGuard(weak_promise);
  let weak_data = rt_weak_add(data_obj);
  let _weak_data_guard = WeakHandleGuard(weak_data);

  // Confirm the promise allocation is in the nursery (so `rt_gc_collect` will evacuate it).
  let mut young_start_pre: *mut u8 = core::ptr::null_mut();
  let mut young_end_pre: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start_pre, &mut young_end_pre);
  }
  assert!(!young_start_pre.is_null());
  assert!(!young_end_pre.is_null());
  assert!(
    (young_start_pre as usize..young_end_pre as usize).contains(&(promise_pre_gc)),
    "promise must start in the nursery so the moving GC relocates it"
  );

  // Keep the test deterministic under parallel execution: STW coordination and the handle-table
  // lock can take much longer in debug builds.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(5)
  };

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table read lock until a stop-the-world request begins.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();

        // Hold the lock long enough for thread C to contend on a write lock and enter a GC-safe
        // region. Once a stop-the-world request is active (epoch is odd), release the lock so the GC
        // coordinator can acquire it for root enumeration.
        let start = Instant::now();
        loop {
          if runtime_native::threading::safepoint::current_epoch() & 1 == 1 {
            break;
          }
          if start.elapsed() > TIMEOUT {
            panic!(
              "timeout waiting for stop-the-world epoch while holding persistent handle table read lock"
            );
          }
          std::thread::yield_now();
        }
      });
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table read lock");

    // Thread C attempts to attach a rooted callback while the lock is held; it should block in
    // `async_rt::gc::Root::new_unchecked` and become `NativeSafe`.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      let id = threading::register_current_thread(threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      // Use a by-value promise pointer so it does not get updated by the moving GC; this reproduces
      // the TOCTOU hazard where `promise_then_rooted` uses `p` after potentially blocking handle-table
      // operations.
      let promise = PromiseRef((promise_pre_gc as *mut u8).cast());
      rt_promise_then_rooted_legacy(promise, record_ptr_and_magic, data_pre_gc as *mut u8);
      c_done_tx.send(()).unwrap();

      threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on a GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
        if t.id() == c_id {
          native_safe = t.is_native_safe();
        }
      });
      if native_safe {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("thread C did not enter a GC-safe region while blocked on the persistent handle table lock");
      }
      std::thread::yield_now();
    }

    // Thread B triggers a moving GC while thread C is blocked.
    let (gc_done_tx, gc_done_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      rt_gc_collect();
      gc_done_tx.send(()).unwrap();
    });

    // While thread B is stopping the world, cooperatively poll safepoints on this harness thread so
    // stop-the-world coordination does not deadlock waiting for us.
    let start = Instant::now();
    loop {
      if gc_done_rx.try_recv().is_ok() {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("timeout waiting for rt_gc_collect to complete");
      }
      threading::safepoint_poll();
      std::thread::yield_now();
    }

    // Wait for thread C to finish the attach call (it should unblock after the GC completes and the
    // lock is released).
    let start = Instant::now();
    loop {
      if c_done_rx.try_recv().is_ok() {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("timeout waiting for rt_promise_then_rooted_legacy to complete after GC");
      }
      threading::safepoint_poll();
      std::thread::yield_now();
    }
  });

  // The weak handles should now resolve to relocated (evacuated) objects.
  let relocated_promise = rt_weak_get(weak_promise);
  assert!(!relocated_promise.is_null());
  let relocated_data = rt_weak_get(weak_data);
  assert!(!relocated_data.is_null());

  // Confirm the objects were evacuated out of the nursery.
  //
  // Use the *current* young-space range (post-GC) rather than any pre-GC snapshot: the GC is free
  // to resize/replace the nursery during collection.
  let mut young_start_post: *mut u8 = core::ptr::null_mut();
  let mut young_end_post: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start_post, &mut young_end_post);
  }
  assert!(!young_start_post.is_null());
  assert!(!young_end_post.is_null());

  assert!(
    !(young_start_post as usize..young_end_post as usize).contains(&(relocated_promise as usize)),
    "expected rt_gc_collect to evacuate the promise out of the nursery"
  );
  assert!(
    !(young_start_post as usize..young_end_post as usize).contains(&(relocated_data as usize)),
    "expected rt_gc_collect to evacuate the data object out of the nursery"
  );

  // Settle the promise and run the queued reaction.
  unsafe {
    rt_promise_fulfill(PromiseRef(relocated_promise.cast()));
  }

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let _ = runtime_native::rt_drain_microtasks();
    let ptr = OBSERVED_PTR.load(Ordering::Acquire);
    if ptr != 0 {
      let magic = OBSERVED_MAGIC.load(Ordering::Relaxed);
      assert_eq!(
        ptr, relocated_data as usize,
        "callback must observe relocated data pointer"
      );
      assert_eq!(magic, MAGIC, "callback must observe correct marker through relocated pointer");
      break;
    }
    assert!(Instant::now() < deadline, "timeout waiting for promise reaction to run");
    std::thread::yield_now();
  }
}
