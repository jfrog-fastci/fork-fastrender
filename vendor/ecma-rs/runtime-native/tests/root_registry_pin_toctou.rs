use runtime_native::gc::GcHeap;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

#[repr(C)]
struct Obj {
  header: runtime_native::gc::ObjHeader,
  value: usize,
}

static OBJ_DESC: runtime_native::TypeDescriptor =
  runtime_native::TypeDescriptor::new(core::mem::size_of::<Obj>(), &[]);

#[test]
fn root_registry_pin_from_raw_pointer_is_moving_gc_safe_under_lock_contention() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Ensure the global root registry starts empty so a minor GC can proceed while a test holds the
  // registry lock.
  assert_eq!(
    runtime_native::roots::global_root_registry().live_count(),
    0,
    "expected no live root registry entries after test runtime reset"
  );

  // Allocate a nursery object that is guaranteed to move during minor GC.
  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&OBJ_DESC);
  unsafe {
    (*(obj as *mut Obj)).value = 0xC0FFEE;
  }

  // Root `obj` in the main thread so we can observe its relocated address after evacuation.
  let ts = threading::registry::current_thread_state().expect("main thread must be registered");
  let scope = runtime_native::gc::RootScope::new(&ts);
  let rooted_obj = scope.root(obj);

  // Raw pointers are `!Send` on newer Rust versions; pass the address as `usize`.
  let obj_addr = obj as usize;

  // Stop-the-world handshakes can take much longer in debug builds (especially
  // under parallel test execution on multi-agent hosts). Keep release builds
  // strict, but give debug builds enough slack to avoid flaky timeouts.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(2)
  };

  std::thread::scope(|scope_threads| {
    // Thread A holds the root registry lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to pin a raw GC pointer while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<u32>();

    scope_threads.spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);

      runtime_native::roots::global_root_registry().debug_with_lock_for_tests(|| {
        // Mark this thread as GC-safe while holding the lock so stop-the-world coordination can
        // proceed even if the thread is blocked on this test channel.
        let gc_safe = threading::enter_gc_safe_region();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(gc_safe);
      });

      threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the root registry lock");

    scope_threads.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let ptr = obj_addr as *mut u8;
      let handle = runtime_native::rt_gc_pin(ptr);
      c_done_tx.send(handle).unwrap();

      threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's pin attempt (it should block on the registry lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it is blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      threading::registry::for_each_thread(|t| {
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

    // Run a moving GC (minor evacuation) while thread C is blocked. This should relocate `obj` and
    // update shadow-stack roots in-place.
    let mut remembered = SimpleRememberedSet::new();
    runtime_native::with_world_stopped(|| {
      heap
        .collect_minor_with_shadow_stacks(&mut remembered)
        .expect("minor GC");
    });

    let relocated = rooted_obj.get();
    assert_ne!(
      relocated as usize, obj_addr,
      "expected the nursery object to be evacuated to a new address during minor GC"
    );
    assert!(
      !heap.is_in_nursery(relocated),
      "expected evacuated object to be out of the nursery"
    );
    unsafe {
      assert_eq!((*(relocated as *const Obj)).value, 0xC0FFEE);
    }

    // Release the lock so thread C can finish pinning the root.
    a_release_tx.send(()).unwrap();

    let handle = c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should finish pinning the root");

    assert_eq!(
      runtime_native::roots::global_root_registry().get(handle),
      Some(relocated),
      "pinned root must resolve to the relocated pointer, not the stale nursery address"
    );

    runtime_native::rt_gc_unpin(handle);
    assert_eq!(
      runtime_native::roots::global_root_registry().live_count(),
      0,
      "unpinning should remove the root registry entry"
    );
  });

  threading::unregister_current_thread();
}

