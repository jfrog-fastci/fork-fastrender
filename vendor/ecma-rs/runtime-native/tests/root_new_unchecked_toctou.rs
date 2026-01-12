use runtime_native::abi::{PromiseRef, RtShapeDescriptor, RtShapeId};
use runtime_native::rt_alloc;
use runtime_native::rt_gc_collect;
use runtime_native::rt_gc_get_young_range;
use runtime_native::rt_promise_fulfill;
use runtime_native::rt_root_pop;
use runtime_native::rt_root_push;
use runtime_native::rt_spawn_blocking_rooted;
use runtime_native::rt_weak_add;
use runtime_native::rt_weak_get;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

const MAGIC: u64 = 0xA0B1_C2D3_E4F5_6789;
const HEADER_SIZE: usize = core::mem::size_of::<runtime_native::gc::ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;

static OBSERVED_PTR: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_MAGIC: AtomicU64 = AtomicU64::new(0);

extern "C" fn record_ptr_and_magic(data: *mut u8, promise: PromiseRef) {
  // Observe the pointer passed through `async_rt::gc::Root`.
  let magic = unsafe { (data.add(MAGIC_OFFSET) as *const u64).read() };
  // Publish the observed magic before the pointer so the test harness can use a single acquire-load
  // of `OBSERVED_PTR` as a completion signal.
  OBSERVED_MAGIC.store(magic, Ordering::Relaxed);
  OBSERVED_PTR.store(data as usize, Ordering::Release);
  unsafe {
    rt_promise_fulfill(promise);
  }
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

struct WeakHandleGuard(u64);
impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
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

#[test]
fn root_new_unchecked_is_safe_under_handle_table_lock_contention() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the current thread claims the event-loop identity so other threads register as `External`
  // rather than becoming the event loop.
  let _ = runtime_native::rt_async_poll();

  ensure_shape_table();

  OBSERVED_PTR.store(0, Ordering::SeqCst);
  OBSERVED_MAGIC.store(0, Ordering::SeqCst);

  // Allocate a young (nursery) object with a recognizable marker.
  let mut obj = rt_alloc(SHAPES[0].size as usize, RtShapeId(1));
  assert!(!obj.is_null());
  unsafe {
    (obj.add(MAGIC_OFFSET) as *mut u64).write(MAGIC);
  }

  // Root the object for the duration of the test so it survives the moving GC.
  unsafe {
    rt_root_push(&mut obj as *mut *mut u8);
  }
  let _root_guard = RootSlotGuard {
    slot: &mut obj as *mut *mut u8,
  };

  // Track relocation via a weak handle.
  let weak = rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  // Confirm the allocation is in the nursery (so `rt_gc_collect` will evacuate it).
  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null());
  assert!(!young_end.is_null());
  assert!((young_start as usize..young_end as usize).contains(&(obj as usize)));

  let obj_usize = obj as usize;

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
            panic!("timeout waiting for stop-the-world epoch while holding persistent handle table read lock");
          }
          std::thread::yield_now();
        }
      });
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table read lock");

    // Thread C attempts to allocate a rooted blocking task while the lock is held; it should block
    // in `async_rt::gc::Root::new_unchecked` and become `NativeSafe`.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<PromiseRef>();
    scope.spawn(move || {
      let id = threading::register_current_thread(threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let ptr = obj_usize as *mut u8;
      let promise = rt_spawn_blocking_rooted(record_ptr_and_magic, ptr);
      c_done_tx.send(promise).unwrap();

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
        panic!(
          "thread C did not enter a GC-safe region while blocked on the persistent handle table lock"
        );
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

    // Wait for thread C to finish the spawn call (it should unblock after the GC completes and the
    // lock is released).
    let start = Instant::now();
    loop {
      if c_done_rx.try_recv().is_ok() {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("timeout waiting for rooted spawn call to complete after GC");
      }
      threading::safepoint_poll();
      std::thread::yield_now();
    }
  });

  // The weak handle should now resolve to the relocated (evacuated) object.
  let relocated = rt_weak_get(weak);
  assert!(!relocated.is_null());
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(
    !(young_start as usize..young_end as usize).contains(&(relocated as usize)),
    "expected rt_gc_collect to evacuate the object out of the nursery"
  );

  // Wait for the blocking task to execute and record the pointer it observed.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let ptr = OBSERVED_PTR.load(Ordering::Acquire);
    if ptr != 0 {
      let magic = OBSERVED_MAGIC.load(Ordering::Relaxed);
      assert_eq!(
        ptr,
        relocated as usize,
        "task must observe the relocated pointer; stale handle indicates TOCTOU in Root::new_unchecked"
      );
      assert_eq!(magic, MAGIC, "task must read the correct marker through the relocated pointer");
      break;
    }
    assert!(Instant::now() < deadline, "timeout waiting for blocking task to run");
    std::thread::yield_now();
  }
}
