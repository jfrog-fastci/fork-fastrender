use core::ptr::NonNull;
use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::rt_alloc;
use runtime_native::rt_gc_collect_minor;
use runtime_native::rt_gc_get_young_range;
use runtime_native::rt_root_pop;
use runtime_native::rt_root_push;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::{HandleId, HandleTable, OwnedAsyncHandle};
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

const MAGIC: u64 = 0xBADC_0FFE_E0DD_F00D;
const HEADER_SIZE: usize = core::mem::size_of::<runtime_native::gc::ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;

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

struct UnregisterThreadOnDrop;
impl Drop for UnregisterThreadOnDrop {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

#[test]
fn handle_table_alloc_movable_is_safe_under_lock_contention() {
  let _rt = TestRuntimeGuard::new();

  // Rooting helpers require the current thread be registered.
  threading::register_current_thread(ThreadKind::Main);
  let _unregister = UnregisterThreadOnDrop;

  ensure_shape_table();

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

  // Confirm the allocation is in the nursery (so `rt_gc_collect_minor` will evacuate it).
  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null());
  assert!(!young_end.is_null());
  assert!((young_start as usize..young_end as usize).contains(&(obj as usize)));

  // Store the pre-GC pointer in a heap allocation so conservative stack scanning can't rewrite it
  // during relocation (see `tests/gc_collect_minor.rs`).
  let obj_before_gc = Box::new(obj as usize);
  let obj_before_gc_usize = *obj_before_gc;

  let table: HandleTable<u8> = HandleTable::new();

  // Keep the test deterministic under parallel execution: STW coordination and lock contention can
  // take much longer in debug builds.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(5)
  };

  std::thread::scope(|scope| {
    let table = &table;

    // Thread A holds the handle table read lock so thread C contends on a write lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      table.debug_with_read_lock_for_tests(move || {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      })
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the handle table read lock");

    // Thread C attempts to allocate an async handle while the lock is held; it should block in
    // `HandleTable::alloc_movable` and become `NativeSafe`.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<HandleId>();
    let (c_finish_tx, c_finish_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let ptr = obj_before_gc_usize as *mut u8;
      let ptr = NonNull::new(ptr).unwrap();
      let owned: OwnedAsyncHandle<'_, u8> = OwnedAsyncHandle::new(table, ptr);
      c_done_tx.send(owned.raw().into()).unwrap();

      // Keep the handle alive until the harness has validated the stored pointer.
      c_finish_rx.recv().unwrap();
      drop(owned);

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
        panic!("thread C did not enter a GC-safe region while blocked on the handle table lock");
      }
      std::thread::yield_now();
    }

    // Thread B triggers a moving GC while thread C is blocked.
    let (gc_done_tx, gc_done_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      rt_gc_collect_minor();
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
          panic!("timeout waiting for rt_gc_collect_minor to complete");
        }
      threading::safepoint_poll();
      std::thread::yield_now();
    }

    // Release the lock so `OwnedAsyncHandle::new` can proceed and read the relocated pointer.
    a_release_tx.send(()).unwrap();

    let handle = c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("handle allocation should complete after lock is released");

    // The rooted pointer should now point to the relocated (evacuated) object.
    let relocated = obj;
    assert!(!relocated.is_null());
    unsafe {
      rt_gc_get_young_range(&mut young_start, &mut young_end);
    }
    assert!(
      !(young_start as usize..young_end as usize).contains(&(relocated as usize)),
      "expected rt_gc_collect_minor to evacuate the object out of the nursery"
    );
    assert_ne!(
      relocated as usize,
      *obj_before_gc,
      "expected moving GC to relocate the object"
    );

    let stored = table
      .get(handle)
      .expect("handle should be live while OwnedAsyncHandle is alive");
    assert_eq!(
      stored.as_ptr(),
      relocated,
      "handle table must store the relocated pointer; stale handle indicates TOCTOU in HandleTable::alloc_movable"
    );
    unsafe {
      let magic = (stored.as_ptr().add(MAGIC_OFFSET) as *const u64).read();
      assert_eq!(magic, MAGIC, "relocated object must retain marker payload");
    }

    // Allow thread C to drop the owning handle and unregister.
    c_finish_tx.send(()).unwrap();
  });
}
