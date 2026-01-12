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
fn register_root_slot_does_not_store_stale_pointer_under_lock_contention() {
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

  // Root the object so it survives the moving GC and so `obj` is updated to the relocated pointer.
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

  // Slot we will register while lock-contended. Allocate it in the Rust heap so conservative stack
  // scanning cannot update it unless we explicitly root it.
  let mut slot_box: Box<*mut u8> = Box::new(obj_before_gc_usize as *mut u8);
  let slot_ptr_usize = (&mut *slot_box as *mut *mut u8) as usize;

  let registry = runtime_native::roots::global_root_registry();
  assert_eq!(
    registry.live_count(),
    0,
    "test assumes global root registry is initially empty so GC will not need to lock it"
  );

  // Keep the test deterministic under parallel execution: STW coordination and lock contention can
  // take much longer in debug builds.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(5)
  };

  std::thread::scope(|scope| {
    // Thread A holds the root registry lock so thread C contends on it.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      registry.debug_with_lock_for_tests(move || {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the root registry lock");

    // Thread C attempts to register the slot as a root while the lock is held; it should block in
    // the GC-aware lock acquisition path and become `NativeSafe`.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<u32>();
    let (c_finish_tx, c_finish_rx) = mpsc::channel::<()>();
    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr_usize as *mut *mut u8;
      let handle = runtime_native::rt_gc_register_root_slot(slot_ptr);
      c_done_tx.send(handle).unwrap();

      c_finish_rx.recv().unwrap();
      runtime_native::rt_gc_unregister_root_slot(handle);

      threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
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
        panic!("thread C did not enter a GC-safe region while blocked on the root registry lock");
      }
      std::thread::yield_now();
    }

    // Trigger a moving GC while thread C is blocked. The slot is not yet registered in the global
    // root registry (so GC won't scan it via `global_root_registry().for_each_root_slot`), but
    // `rt_gc_register_root_slot` must still ensure the slot value is kept relocatable across this
    // window.
    rt_gc_collect_minor();

    // Release the lock so `rt_gc_register_root_slot` can proceed and publish the relocated value.
    a_release_tx.send(()).unwrap();

    let handle = c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("root registration should complete after lock is released");
    assert_ne!(handle, 0);

    // Re-read the young range after collection (debug builds may conservatively mutate stack locals).
    unsafe {
      rt_gc_get_young_range(&mut young_start, &mut young_end);
    }

    let relocated = obj;
    assert!(
      !(young_start as usize..young_end as usize).contains(&(relocated as usize)),
      "expected rt_gc_collect_minor to evacuate the object out of the nursery"
    );
    assert_ne!(
      relocated as usize, *obj_before_gc,
      "expected moving GC to relocate the object"
    );

    assert_eq!(
      *slot_box as usize, relocated as usize,
      "rt_gc_register_root_slot must not leave the slot pointing at a stale pre-relocation address"
    );
    unsafe {
      let magic = (relocated.add(MAGIC_OFFSET) as *const u64).read();
      assert_eq!(magic, MAGIC, "relocated object must retain marker payload");
    }

    // Allow thread C to unregister the root and unregister itself.
    c_finish_tx.send(()).unwrap();
  });
}

